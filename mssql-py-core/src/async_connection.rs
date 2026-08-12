// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous connection API for the Core TDS backend.
//!
//! # ⚠️ Preview API — unstable
//!
//! The types and methods in this module are **not** part of the stable
//! `mssql-py-core` surface. Signatures, error behavior, and internal
//! semantics may change without notice in any release. First use in a
//! Python process emits a [`FutureWarning`] via `warnings.warn`.
//!
//! Sibling of `connection.rs` (the synchronous surface). Every type defined
//! here submits its I/O to the shared process-wide Tokio runtime via
//! [`crate::async_runtime`] and returns Python awaitables through
//! `pyo3_async_runtimes::tokio::future_into_py`, so callers can `await` the
//! results from `asyncio`.
//!
//! Invariant: one async connection maps to exactly one async cursor, one
//! `TdsClient`, and one TDS wire session.
//!
//! [`FutureWarning`]: https://docs.python.org/3/library/exceptions.html#FutureWarning

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::exceptions::{PyFutureWarning, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};
use tokio::sync::Mutex;

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;

use crate::async_cursor::PyAsyncCursor;
use crate::connection::PyCoreConnection;

/// Emit a `FutureWarning` the first time any async API is exercised in this
/// process. Silenceable by callers via `warnings.filterwarnings(...)`.
static PREVIEW_WARNED: AtomicBool = AtomicBool::new(false);

fn emit_preview_warning(py: Python<'_>) -> PyResult<()> {
    if PREVIEW_WARNED.load(Ordering::Acquire) {
        return Ok(());
    }
    let category = py.get_type::<PyFutureWarning>();
    // stacklevel=1: native methods have no Python frame, so 1 lands on the caller's `connect(...)`.
    PyErr::warn(
        py,
        &category,
        c"mssql_py_core async API is a preview and subject to breaking changes without notice; do not depend on it from production code.",
        1,
    )?;
    PREVIEW_WARNED.store(true, Ordering::Release);
    Ok(())
}

/// Asynchronous Python connection backed by the Core TDS client.
///
/// # ⚠️ Preview API — unstable
///
/// Preview surface: API, method signatures, error behavior, and internal
/// semantics may change without notice in minor releases. Do not depend on
/// it from production code.
///
/// Instances are created via [`PyAsyncConnection::connect`], which returns a
/// Python awaitable. The awaitable resolves on the caller's `asyncio` loop
/// once the TCP + TLS + login handshake has completed on the shared Tokio
/// runtime.
///
/// TODO(User Story 47180 [mssql-python] Cancel API and Cancellation Bridge):
/// cancellation of a suspended `commit`, `rollback`, or `close` future can
/// desync the TDS byte stream (bytes written, response not yet read), so a
/// subsequent operation on the same connection may read a stale response
/// and corrupt the wire. Callers must not cancel these awaitables against a
/// connection they intend to keep using. Cancellation-safe semantics are
/// tracked at
/// <https://sqlclientdrivers.visualstudio.com/mssql-python/_workitems/edit/47180>.
#[pyclass]
pub struct PyAsyncConnection {
    /// Wrapped in `Option` so `close()` can take ownership and drop the
    /// client; wrapped in `Arc<tokio::sync::Mutex<...>>` so the (upcoming)
    /// async cursor and connection-level lifecycle methods can share access
    /// across `.await` points without corrupting the TDS byte stream.
    tds_client: Option<Arc<Mutex<TdsClient>>>,
}

#[pymethods]
impl PyAsyncConnection {
    /// Establish a TDS connection asynchronously.
    ///
    /// ```python
    /// conn = await PyAsyncConnection.connect(client_context_dict)
    /// ```
    ///
    /// Dictionary parsing runs synchronously on the calling thread (it needs
    /// the GIL). The network handshake is submitted to the shared Tokio
    /// runtime and driven concurrently with the caller's asyncio loop.
    #[classmethod]
    fn connect<'py>(
        cls: &Bound<'py, PyType>,
        client_context_dict: &Bound<'_, PyDict>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = cls.py();

        // Preview API: emit a one-shot FutureWarning so callers see the
        // instability signal at runtime.
        emit_preview_warning(py)?;

        tracing::info!("PyAsyncConnection::connect: extracting client context");
        let context = PyCoreConnection::dict_to_client_context(client_context_dict)?;
        let datasource = context.data_source.clone();

        tracing::info!(
            "PyAsyncConnection::connect: encryption mode={:?}, trust_server_certificate={}, host_name_in_cert={:?}",
            context.encryption_options.mode,
            context.encryption_options.trust_server_certificate,
            context.encryption_options.host_name_in_cert,
        );

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tracing::info!(
                "PyAsyncConnection::connect: opening TDS connection to {}",
                datasource
            );
            let provider = TdsConnectionProvider {};
            let client = provider
                .create_client(context, &datasource, None)
                .await
                .map_err(|e| {
                    tracing::error!("PyAsyncConnection::connect: failed: {}", e);
                    PyRuntimeError::new_err(format!("Failed to connect to SQL Server: {e}"))
                })?;

            tracing::info!("PyAsyncConnection::connect: connection established");
            Python::attach(|py| {
                Py::new(
                    py,
                    PyAsyncConnection {
                        tds_client: Some(Arc::new(Mutex::new(client))),
                    },
                )
            })
        })
    }

    /// Close the TDS connection asynchronously.
    ///
    /// ```python
    /// await conn.close()
    /// ```
    ///
    /// Sends the TDS logout token and tears down the underlying transport.
    /// The awaitable is submitted to the shared Tokio runtime so the calling
    /// asyncio loop stays unblocked while the graceful shutdown runs.
    ///
    /// Idempotent: awaiting `close()` on an already-closed connection
    /// resolves immediately with no I/O. If the graceful shutdown itself
    /// errors, the error is logged at `warn` level and the connection is
    /// still considered closed — the OS closes the socket on drop either
    /// way, so we never leak the resource.
    fn close<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Detach the client from `self` synchronously (while `&mut self` is
        // valid) so the future can own it for `'static + Send`. Subsequent
        // method calls on this connection will see `tds_client == None`.
        let client_opt = self.tds_client.take();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let Some(client) = client_opt else {
                tracing::debug!("PyAsyncConnection::close: already closed, no-op");
                return Ok(());
            };

            tracing::info!(
                "PyAsyncConnection::close: sending TDS logout and tearing down transport"
            );
            let mut guard = client.lock().await;
            if let Err(e) = guard.close_connection().await {
                // Match sync-path semantics: log and swallow. The connection
                // is treated as closed regardless — the transport will be
                // dropped when the Arc's last reference goes away.
                tracing::warn!(
                    "PyAsyncConnection::close: error during graceful shutdown: {}",
                    e
                );
            }
            Ok(())
        })
    }

    /// Commit the current TDS transaction asynchronously.
    ///
    /// ```python
    /// await conn.commit()
    /// ```
    ///
    /// Sends a TM_COMMIT (Transaction Manager COMMIT) request over the wire
    /// and awaits the server's DONE token. Raises `RuntimeError`
    /// synchronously if the connection has already been closed.
    ///
    /// If no transaction is currently open on the server, the commit will
    /// fail with the server's own error (SQL Server 3902 — "The COMMIT
    /// TRANSACTION request has no corresponding BEGIN TRANSACTION").
    fn commit<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Clone the Arc synchronously so the future is `'static + Send`
        // without borrowing `self`. Only a shared borrow is required —
        // nothing on `self` is mutated here.
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Connection is closed"))?
            .clone();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tracing::info!("PyAsyncConnection::commit: sending TM_COMMIT");
            let mut guard = client.lock().await;
            guard.commit_transaction(None, None).await.map_err(|e| {
                tracing::error!("PyAsyncConnection::commit: failed: {}", e);
                PyRuntimeError::new_err(format!("Commit failed: {e}"))
            })?;
            tracing::info!("PyAsyncConnection::commit: transaction committed");
            Ok(())
        })
    }

    /// Roll back the current TDS transaction asynchronously.
    ///
    /// ```python
    /// await conn.rollback()
    /// ```
    ///
    /// Sends a TM_ROLLBACK (Transaction Manager ROLLBACK) request over the
    /// wire and awaits the server's DONE token. Raises `RuntimeError`
    /// synchronously if the connection has already been closed.
    ///
    /// If no transaction is currently open on the server, the rollback will
    /// fail with the server's own error (SQL Server 3903 — "The ROLLBACK
    /// TRANSACTION request has no corresponding BEGIN TRANSACTION").
    fn rollback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Clone the Arc synchronously so the future is `'static + Send`
        // without borrowing `self`. Only a shared borrow is required —
        // nothing on `self` is mutated here.
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Connection is closed"))?
            .clone();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tracing::info!("PyAsyncConnection::rollback: sending TM_ROLLBACK");
            let mut guard = client.lock().await;
            guard.rollback_transaction(None, None).await.map_err(|e| {
                tracing::error!("PyAsyncConnection::rollback: failed: {}", e);
                PyRuntimeError::new_err(format!("Rollback failed: {e}"))
            })?;
            tracing::info!("PyAsyncConnection::rollback: transaction rolled back");
            Ok(())
        })
    }

    /// Create an async cursor bound to this connection.
    ///
    /// ```python
    /// cur = conn.cursor()
    /// await cur.execute("SELECT 1")
    /// ```
    ///
    /// This method does not perform I/O — it simply hands out a new
    /// [`PyAsyncCursor`] that shares the connection's `TdsClient` via an
    /// `Arc<tokio::sync::Mutex<_>>`. Following DB-API 2.0, `cursor()` is a
    /// synchronous call; only the cursor's execute/fetch methods will be
    /// awaitable.
    ///
    /// Raises `RuntimeError` if the connection has already been closed.
    /// A second cursor may be created on the same connection, but both
    /// cursors share one TDS wire session and serialize on the same async
    /// mutex — matching the non-MARS TDS session model.
    fn cursor(&self) -> PyResult<PyAsyncCursor> {
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Connection is closed"))?
            .clone();
        Ok(PyAsyncCursor::new(client))
    }
}

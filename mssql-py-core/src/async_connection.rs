// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous connection API for the Core TDS backend.
//!
//! Sibling of `connection.rs` (the synchronous surface). Every type defined
//! here submits its I/O to the shared process-wide Tokio runtime via
//! [`crate::async_runtime`] and returns Python awaitables through
//! `pyo3_async_runtimes::tokio::future_into_py`, so callers can `await` the
//! results from `asyncio`.
//!
//! Invariant: one async connection maps to exactly one async cursor, one
//! `TdsClient`, and one TDS wire session.

use std::sync::Arc;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};
use tokio::sync::Mutex;

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;

use crate::async_cursor::PyAsyncCursor;
use crate::connection::PyCoreConnection;

/// Asynchronous Python connection backed by the Core TDS client.
///
/// Instances are created via [`PyAsyncConnection::connect`], which returns a
/// Python awaitable. The awaitable resolves on the caller's `asyncio` loop
/// once the TCP + TLS + login handshake has completed on the shared Tokio
/// runtime.
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
    fn commit<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Clone the Arc synchronously so the future is `'static + Send`
        // without borrowing `self`. The `&mut self` borrow is released as
        // soon as this method returns.
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
    fn rollback<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Clone the Arc synchronously so the future is `'static + Send`
        // without borrowing `self`. The `&mut self` borrow is released as
        // soon as this method returns.
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

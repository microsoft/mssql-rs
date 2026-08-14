// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous connection API for the Core TDS backend.
//!
//! Preview API — unstable. First use emits a `FutureWarning`.
//!
//! Invariant: one async connection ↔ one async cursor ↔ one `TdsClient`.

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
use crate::python_logger_adapter::scoped_tracing_bridge;

/// One-shot `FutureWarning` per process; silenceable via `warnings.filterwarnings`.
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
/// Preview API — unstable.
///
/// TODO(User Story 47180 [mssql-python] Cancel API and Cancellation Bridge):
/// cancellation of a suspended `commit`, `rollback`, or `close` future can
/// desync the TDS byte stream. Callers must not cancel these awaitables
/// against a connection they intend to keep using.
/// <https://sqlclientdrivers.visualstudio.com/mssql-python/_workitems/edit/47180>
#[pyclass]
pub struct PyAsyncConnection {
    /// `Option` so `close()` can `take()`; `Arc<Mutex<>>` for cursor sharing.
    tds_client: Option<Arc<Mutex<TdsClient>>>,
}

#[pymethods]
impl PyAsyncConnection {
    /// Establish a TDS connection. Dict parsing is synchronous; the network
    /// handshake runs on the shared Tokio runtime.
    #[classmethod]
    #[pyo3(signature = (client_context_dict, python_logger=None))]
    fn connect<'py>(
        cls: &Bound<'py, PyType>,
        client_context_dict: &Bound<'_, PyDict>,
        python_logger: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = cls.py();

        // `DefaultGuard` is `!Send`, so its coverage ends when this method returns.
        let _guard = python_logger
            .map(|logger| scoped_tracing_bridge(Arc::new(logger.clone().unbind()), file!()));

        emit_preview_warning(py)?;

        tracing::info!("PyAsyncConnection::connect: initiating async connection");

        tracing::info!("PyAsyncConnection::connect: extracting client context");
        let context = PyCoreConnection::dict_to_client_context(client_context_dict)?;
        let datasource = context.data_source.clone();

        tracing::info!(
            "PyAsyncConnection::connect: encryption mode={:?}, trust_server_certificate={}, host_name_in_cert={:?}, server_certificate={:?}",
            context.encryption_options.mode,
            context.encryption_options.trust_server_certificate,
            context.encryption_options.host_name_in_cert,
            context.encryption_options.server_certificate,
        );

        tracing::info!(
            "PyAsyncConnection::connect: authentication method={:?}",
            context.tds_authentication_method,
        );

        tracing::info!(
            "PyAsyncConnection::connect: attempting connection to datasource: {}",
            datasource
        );

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let provider = TdsConnectionProvider {};
            let client = provider
                .create_client(context, &datasource, None)
                .await
                .map_err(|e| {
                    tracing::error!("PyAsyncConnection::connect: failed: {}", e);
                    // TODO(User Story 47181): map TdsError to a DB-API-compliant exception, preserving SQLSTATE + server error number.
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

    /// Close the connection. Idempotent. Shutdown errors are logged and swallowed.
    fn close<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        tracing::info!("PyAsyncConnection::close: initiating close");
        // `take()` before spawning: gives the future 'static ownership; marks conn closed.
        let client_opt = self.tds_client.take();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let Some(client) = client_opt else {
                tracing::debug!("PyAsyncConnection::close: already closed, no-op");
                return Python::attach(|py| Ok(py.None()));
            };

            tracing::info!(
                "PyAsyncConnection::close: sending TDS logout and tearing down transport"
            );
            let mut guard = client.lock().await;
            if let Err(e) = guard.close_connection().await {
                // Log and swallow; connection is closed regardless.
                tracing::warn!(
                    "PyAsyncConnection::close: error during graceful shutdown: {}",
                    e
                );
            }
            tracing::info!("PyAsyncConnection::close: connection closed");
            Python::attach(|py| Ok(py.None()))
        })
    }

    /// Commit the current transaction. If none is open, surfaces SQL Server 3902.
    fn commit<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Clone the Arc so the future is `'static + Send`.
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
                // TODO(User Story 47181): map TdsError to a DB-API-compliant exception, preserving SQLSTATE + server error number.
                PyRuntimeError::new_err(format!("Commit failed: {e}"))
            })?;
            tracing::info!("PyAsyncConnection::commit: transaction committed");
            Python::attach(|py| Ok(py.None()))
        })
    }

    /// Roll back the current transaction. If none is open, surfaces SQL Server 3903.
    fn rollback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Clone the Arc so the future is `'static + Send`.
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
                // TODO(User Story 47181): map TdsError to a DB-API-compliant exception, preserving SQLSTATE + server error number.
                PyRuntimeError::new_err(format!("Rollback failed: {e}"))
            })?;
            tracing::info!("PyAsyncConnection::rollback: transaction rolled back");
            Python::attach(|py| Ok(py.None()))
        })
    }

    /// Sync per DB-API 2.0. A second cursor is allowed; both share the same
    /// TDS session and serialize on the same async mutex.
    fn cursor(&self) -> PyResult<PyAsyncCursor> {
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Connection is closed"))?
            .clone();
        Ok(PyAsyncCursor::new(client))
    }
}

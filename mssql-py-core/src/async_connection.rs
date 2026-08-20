// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous connection API for the Core TDS backend.
//!
//! Preview API — unstable. First use emits a `FutureWarning`.
//!
//! Invariant: one `TdsClient` and one TDS wire session per async connection.
//! All cursors share that client and serialize access through the async mutex.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::exceptions::{PyFutureWarning, PyOverflowError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;

use crate::async_cursor::PyAsyncCursor;
use crate::async_session::AsyncConnectionState;
use crate::connection::PyCoreConnection;
use crate::python_logger_adapter::python_logger_dispatch;

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

/// Map a TDS error to a Python exception with per-operation context.
///
/// TODO(User Story 47181): map TdsError to a DB-API-compliant exception,
/// preserving SQLSTATE + server error number.
/// <https://sqlclientdrivers.visualstudio.com/mssql-python/_workitems/edit/47181>
fn map_tds_error(op: &str, user_msg: &str, e: impl std::fmt::Display) -> PyErr {
    tracing::error!("PyAsyncConnection::{op}: failed: {e}");
    PyRuntimeError::new_err(format!("{user_msg}: {e}"))
}

async fn with_optional_dispatch<F>(future: F, dispatch: Option<tracing::Dispatch>) -> F::Output
where
    F: Future,
{
    match dispatch {
        Some(dispatch) => future.with_subscriber(dispatch).await,
        None => future.await,
    }
}

async fn close_client(client: Arc<Mutex<TdsClient>>, autocommit: bool) -> bool {
    let mut guard = client.lock().await;
    if !autocommit
        && guard.has_active_transaction()
        && let Err(error) = guard.rollback_transaction(None, None).await
    {
        tracing::warn!(
            "PyAsyncConnection::close: failed to roll back active transaction: {}",
            error
        );
    }
    match guard.close_connection().await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "PyAsyncConnection::close: error during graceful shutdown: {}",
                error
            );
            false
        }
    }
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
    tracing_dispatch: Option<tracing::Dispatch>,
    /// Shared now so future cursor execution observes the connection mode.
    autocommit: Arc<AtomicBool>,
    session_state: Arc<AsyncConnectionState>,
    /// Default query timeout (seconds) applied to cursors created from this
    /// connection. `0` = no timeout, per pyodbc/ODBC `SQL_ATTR_QUERY_TIMEOUT`.
    /// Pure Python-side state — the setter performs no I/O.
    default_query_timeout: u32,
}

#[pymethods]
impl PyAsyncConnection {
    /// Establish a TDS connection. Dict parsing is synchronous; the network
    /// handshake runs on the shared Tokio runtime.
    #[classmethod]
    #[pyo3(signature = (client_context_dict, python_logger=None, autocommit=false))]
    fn connect<'py>(
        cls: &Bound<'py, PyType>,
        client_context_dict: &Bound<'_, PyDict>,
        python_logger: Option<&Bound<'_, PyAny>>,
        autocommit: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = cls.py();

        let dispatch = python_logger
            .map(|logger| python_logger_dispatch(Arc::new(logger.clone().unbind()), file!()));
        let _guard = dispatch.as_ref().map(tracing::dispatcher::set_default);

        emit_preview_warning(py)?;

        tracing::debug!("PyAsyncConnection::connect: extracting client context");
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

        let future_dispatch = dispatch.clone();
        pyo3_async_runtimes::tokio::future_into_py(
            py,
            with_optional_dispatch(
                async move {
                    let provider = TdsConnectionProvider {};
                    let client = provider
                        .create_client(context, &datasource, None)
                        .await
                        .map_err(|e| {
                            map_tds_error("connect", "Failed to connect to SQL Server", e)
                        })?;

                    tracing::info!("PyAsyncConnection::connect: connection established");
                    Python::attach(|py| {
                        Py::new(
                            py,
                            PyAsyncConnection {
                                tds_client: Some(Arc::new(Mutex::new(client))),
                                tracing_dispatch: dispatch,
                                autocommit: Arc::new(AtomicBool::new(autocommit)),
                                session_state: Arc::new(AsyncConnectionState::new()),
                                default_query_timeout: 0,
                            },
                        )
                    })
                },
                future_dispatch,
            ),
        )
    }

    /// Default query timeout (seconds) inherited by cursors created from this
    /// connection. `0` means no timeout.
    #[getter]
    fn timeout(&self) -> u32 {
        self.default_query_timeout
    }

    /// Set the default query timeout (seconds) for future cursors. Existing
    /// cursors and in-flight queries are unaffected. Negative values raise
    /// `ValueError`; values above `u32::MAX` raise `OverflowError`.
    #[setter]
    fn set_timeout(&mut self, value: i64) -> PyResult<()> {
        if value < 0 {
            return Err(PyValueError::new_err("Timeout cannot be negative"));
        }
        let value = u32::try_from(value)
            .map_err(|_| PyOverflowError::new_err("Timeout exceeds maximum supported value"))?;

        let _guard = self
            .tracing_dispatch
            .as_ref()
            .map(tracing::dispatcher::set_default);
        tracing::info!(
            "PyAsyncConnection::set_timeout: default query timeout set to {}s",
            value
        );
        self.default_query_timeout = value;
        Ok(())
    }

    /// Whether statements use SQL Server autocommit mode. The mode is fixed at
    /// connection time until async transition semantics are implemented.
    #[getter]
    fn autocommit(&self) -> bool {
        self.autocommit.load(Ordering::Relaxed)
    }

    /// True after `close()` has been called. Cheap LBYL check; performs no I/O.
    #[getter]
    fn closed(&self) -> bool {
        self.tds_client.is_none()
    }

    /// Inverse of `.closed`, provided for sync-path parity with `PyCoreConnection`.
    fn is_connected(&self) -> bool {
        self.tds_client.is_some()
    }

    fn __repr__(&self) -> &'static str {
        if self.tds_client.is_none() {
            "PyAsyncConnection(closed)"
        } else {
            "PyAsyncConnection(connected)"
        }
    }

    /// Close the connection. Rolls back active work when autocommit is disabled.
    /// Idempotent; rollback and shutdown errors are logged and swallowed.
    /// Returns an awaitable that resolves to `None`.
    fn close<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let dispatch = self.tracing_dispatch.clone();
        let _guard = dispatch.as_ref().map(tracing::dispatcher::set_default);
        tracing::info!("PyAsyncConnection::close: initiating close");
        // `take()` before spawning: gives the future 'static ownership; marks conn closed.
        let client_opt = self.tds_client.take();
        let autocommit = self.autocommit.load(Ordering::Relaxed);
        let session_state = self.session_state.clone();
        if client_opt.is_some() {
            session_state.begin_close();
        }

        pyo3_async_runtimes::tokio::future_into_py(
            py,
            with_optional_dispatch(
                async move {
                    let Some(client) = client_opt else {
                        tracing::debug!("PyAsyncConnection::close: already closed, no-op");
                        return Python::attach(|py| Ok(py.None()));
                    };

                    tracing::info!(
                        "PyAsyncConnection::close: sending TDS logout and tearing down transport"
                    );
                    if close_client(client, autocommit).await {
                        session_state.mark_closed();
                    } else {
                        session_state.mark_broken();
                    }
                    tracing::info!("PyAsyncConnection::close: connection closed");
                    Python::attach(|py| Ok(py.None()))
                },
                dispatch,
            ),
        )
    }

    /// Async context manager entry. Resolves to `self` with no I/O.
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        if slf.borrow(py).tds_client.is_none() {
            return Err(PyRuntimeError::new_err("Connection is closed"));
        }
        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(slf) })
    }

    /// Commit on clean exit or roll back on exceptional exit when autocommit is
    /// disabled, then always close. Cleanup never masks the block's exception.
    fn __aexit__<'py>(
        &mut self,
        py: Python<'py>,
        exc_type: &Bound<'_, PyAny>,
        _exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client_opt = self.tds_client.take();
        let autocommit = self.autocommit.load(Ordering::Relaxed);
        let has_block_error = !exc_type.is_none();
        let dispatch = self.tracing_dispatch.clone();
        let session_state = self.session_state.clone();
        if client_opt.is_some() {
            session_state.begin_close();
        }

        pyo3_async_runtimes::tokio::future_into_py(
            py,
            with_optional_dispatch(
                async move {
                    let Some(client) = client_opt else {
                        session_state.mark_closed();
                        return Python::attach(|py| Ok(py.None()));
                    };

                    let mut guard = client.lock().await;
                    let finalize_result = if !autocommit && guard.has_active_transaction() {
                        if has_block_error {
                            guard
                                .rollback_transaction(None, None)
                                .await
                                .map_err(|error| {
                                    map_tds_error(
                                        "__aexit__",
                                        "Rollback on context exit failed",
                                        error,
                                    )
                                })
                        } else {
                            guard.commit_transaction(None, None).await.map_err(|error| {
                                map_tds_error("__aexit__", "Commit on context exit failed", error)
                            })
                        }
                    } else {
                        Ok(())
                    };

                    let close_result = guard.close_connection().await.map_err(|error| {
                        map_tds_error("__aexit__", "Close on context exit failed", error)
                    });
                    if close_result.is_ok() {
                        session_state.mark_closed();
                    } else {
                        session_state.mark_broken();
                    }

                    if has_block_error {
                        if let Err(error) = finalize_result {
                            tracing::warn!(
                                "PyAsyncConnection::__aexit__: cleanup failed while preserving block exception: {}",
                                error
                            );
                        }
                        if let Err(error) = close_result {
                            tracing::warn!(
                                "PyAsyncConnection::__aexit__: close failed while preserving block exception: {}",
                                error
                            );
                        }
                        return Python::attach(|py| Ok(py.None()));
                    }

                    finalize_result?;
                    close_result?;
                    Python::attach(|py| Ok(py.None()))
                },
                dispatch,
            ),
        )
    }

    /// Commit the current transaction. No-op if none is active.
    /// Raises `RuntimeError` synchronously if the connection is closed.
    fn commit<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Clone the Arc so the future is `'static + Send`.
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Connection is closed"))?
            .clone();
        let dispatch = self.tracing_dispatch.clone();

        pyo3_async_runtimes::tokio::future_into_py(
            py,
            with_optional_dispatch(
                async move {
                    let mut guard = client.lock().await;
                    if !guard.has_active_transaction() {
                        return Python::attach(|py| Ok(py.None()));
                    }
                    tracing::info!("PyAsyncConnection::commit: sending TM_COMMIT");
                    guard
                        .commit_transaction(None, None)
                        .await
                        .map_err(|e| map_tds_error("commit", "Commit failed", e))?;
                    tracing::info!("PyAsyncConnection::commit: transaction committed");
                    Python::attach(|py| Ok(py.None()))
                },
                dispatch,
            ),
        )
    }

    /// Roll back the current transaction. No-op if none is active.
    /// Raises `RuntimeError` synchronously if the connection is closed.
    fn rollback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Clone the Arc so the future is `'static + Send`.
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Connection is closed"))?
            .clone();
        let dispatch = self.tracing_dispatch.clone();

        pyo3_async_runtimes::tokio::future_into_py(
            py,
            with_optional_dispatch(
                async move {
                    let mut guard = client.lock().await;
                    if !guard.has_active_transaction() {
                        return Python::attach(|py| Ok(py.None()));
                    }
                    tracing::info!("PyAsyncConnection::rollback: sending TM_ROLLBACK");
                    guard
                        .rollback_transaction(None, None)
                        .await
                        .map_err(|e| map_tds_error("rollback", "Rollback failed", e))?;
                    tracing::info!("PyAsyncConnection::rollback: transaction rolled back");
                    Python::attach(|py| Ok(py.None()))
                },
                dispatch,
            ),
        )
    }

    /// Create an async cursor. Sync per DB-API 2.0. Raises `RuntimeError` if the
    /// connection is closed. Additional cursors are allowed; all share the same
    /// TDS session and serialize on the same async mutex.
    fn cursor(&self) -> PyResult<PyAsyncCursor> {
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Connection is closed"))?
            .clone();
        Ok(PyAsyncCursor::new(
            client,
            self.tracing_dispatch.clone(),
            self.autocommit.clone(),
            self.session_state.clone(),
            self.session_state.allocate_cursor_id(),
            self.default_query_timeout,
        ))
    }
}

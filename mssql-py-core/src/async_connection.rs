// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous connection API for the Core TDS backend.
//!
//! Preview API — unstable. First use emits a `FutureWarning`.
//!
//! Invariant: one `TdsClient` and one TDS wire session per async connection.
//! Cursors retain a logical parent whose physical handle is revoked on close.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::exceptions::{PyFutureWarning, PyOverflowError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};
use tracing::instrument::WithSubscriber;

use mssql_tds::connection::client_context::TdsAuthenticationMethod;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::error::Error as TdsError;

use crate::async_cursor::PyAsyncCursor;
use crate::async_entra_credentials::BuiltInEntraCredential;
use crate::async_entra_token_factory::AsyncPythonEntraIdTokenFactory;
use crate::async_errors::{InterfaceError, map_connection_claim};
use crate::async_handle::{CLOSE_TIMEOUT, CloseMode, LogicalConnection};
use crate::async_session::{AsyncConnectionState, ClaimError, OperationId};
use crate::connection::PyCoreConnection;
use crate::python_logger_adapter::python_logger_dispatch;

/// One-shot `FutureWarning` per process; silenceable via `warnings.filterwarnings`.
static PREVIEW_WARNED: AtomicBool = AtomicBool::new(false);

fn validate_custom_token_context(dict: &Bound<'_, PyDict>) -> PyResult<()> {
    let optional_string = |key| -> PyResult<Option<String>> {
        dict.get_item(key)?
            .filter(|value| !value.is_none())
            .map(|value| {
                value
                    .extract()
                    .map_err(|_| InterfaceError::new_err(format!("{key} must be a string or None")))
            })
            .transpose()
    };
    let trusted_connection = optional_string("trusted_connection")?;
    let trusted_connection = match trusted_connection.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("yes") => Some(true),
        Some(value) if value.eq_ignore_ascii_case("no") => Some(false),
        None => None,
        Some(_) => {
            return Err(InterfaceError::new_err(
                "Trusted_Connection must be 'Yes' or 'No'",
            ));
        }
    };
    let mut conflicts = Vec::new();
    if trusted_connection == Some(true) {
        conflicts.push("trusted_connection");
    }
    for key in ["authentication", "user_name", "password"] {
        if optional_string(key)?.is_some_and(|value| !value.is_empty()) {
            conflicts.push(key);
        }
    }
    for key in ["access_token", "entra_id_token_factory"] {
        if dict.get_item(key)?.is_some_and(|value| !value.is_none()) {
            conflicts.push(key);
        }
    }
    if !conflicts.is_empty() {
        return Err(InterfaceError::new_err(format!(
            "token_provider cannot be combined with: {}",
            conflicts.join(", ")
        )));
    }
    Ok(())
}

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

fn map_connect_error(error: TdsError, uses_async_auth: bool) -> PyErr {
    if matches!(error, TdsError::TokenAcquisitionCancelled)
        || (uses_async_auth && matches!(error, TdsError::OperationCancelledError(_)))
    {
        tracing::debug!("PyAsyncConnection::connect: token acquisition cancelled");
        return Python::attach(|py| {
            py.import("asyncio")?
                .getattr("CancelledError")?
                .call0()
                .map(PyErr::from_value)
        })
        .unwrap_or_else(|error| error);
    }
    tracing::error!("PyAsyncConnection::connect: failed: {error}");
    crate::async_errors::map_tds_error("Failed to connect to SQL Server", error, Vec::new())
}

fn map_claim_error(error: ClaimError) -> PyErr {
    map_connection_claim(error, "Connection is busy with another operation")
}

/// Owns the session claim for an in-flight commit or rollback.
///
/// `complete()` releases the claim after success or a handled TDS error. Dropping
/// before completion indicates cancellation or an interrupted protocol exchange,
/// so the connection is marked broken before the claim is released.
struct ConnectionOperationGuard {
    session_state: Arc<AsyncConnectionState>,
    operation_id: OperationId,
    completed: bool,
}

impl ConnectionOperationGuard {
    /// Creates a guard for an already-acquired connection-operation claim.
    fn new(session_state: Arc<AsyncConnectionState>, operation_id: OperationId) -> Self {
        Self {
            session_state,
            operation_id,
            completed: false,
        }
    }

    /// Releases the claim and prevents drop-time connection poisoning.
    fn complete(&mut self) {
        self.session_state.release_operation(self.operation_id);
        self.completed = true;
    }
}

impl Drop for ConnectionOperationGuard {
    /// Marks the connection broken when an operation is cancelled or interrupted.
    fn drop(&mut self) {
        if !self.completed {
            self.session_state.mark_broken();
            self.session_state.release_operation(self.operation_id);
        }
    }
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

/// Asynchronous Python connection backed by the Core TDS client.
///
/// This API is a preview and may change between minor releases. Cancelling
/// commit or rollback invalidates the connection. Cancelling a close waiter
/// does not stop bounded cleanup; another close waits for the same outcome.
#[pyclass]
pub struct PyAsyncConnection {
    /// Removing this wrapper's reference does not revoke surviving cursor owners.
    tds_client: Option<Arc<LogicalConnection>>,
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
    /// Establish a connection and return an awaitable resolving to it.
    ///
    /// `client_context_dict` supplies the SQL Server connection options.
    /// `autocommit` defaults to `False`.
    /// `token_provider` optionally supplies a caller-owned async credential whose
    /// `get_token(scope)` awaitable returns an object with a non-empty `token`
    /// string. It cannot be combined with other authentication sources. The
    /// driver never closes the provider.
    /// Built-in ActiveDirectoryDefault, ActiveDirectoryMSI, and client-secret
    /// ActiveDirectoryServicePrincipal use azure.identity.aio. They require the
    /// azure-async extra and support Azure Public Cloud only. Driver-created
    /// credentials are closed after each acquisition attempt.
    /// Built-in Default requests the validated FedAuth tenant; custom providers
    /// retain their own tenant policy and receive only the scope argument.
    /// Acquisition shares the native login deadline. Owned cleanup has a
    /// separate five-second settlement limit; a cleanup timeout fails login
    /// even if the close coroutine suppresses cancellation. Keep the originating
    /// loop running until credential tasks have settled.
    /// Cancelled connect futures cannot publish tokens from queued acquisition
    /// or credential-close completions.
    /// TDS connect failures use DB-API exceptions regardless of authentication
    /// mode; async Entra cancellation raises asyncio.CancelledError.
    #[classmethod]
    #[pyo3(signature = (client_context_dict, python_logger=None, autocommit=false, *, token_provider=None))]
    fn connect<'py>(
        cls: &Bound<'py, PyType>,
        client_context_dict: &Bound<'_, PyDict>,
        python_logger: Option<&Bound<'_, PyAny>>,
        autocommit: bool,
        token_provider: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = cls.py();

        let dispatch = python_logger
            .map(|logger| python_logger_dispatch(Arc::new(logger.clone().unbind()), file!()));
        let _guard = dispatch.as_ref().map(tracing::dispatcher::set_default);

        emit_preview_warning(py)?;

        tracing::debug!("PyAsyncConnection::connect: extracting client context");
        let mut uses_async_auth = token_provider.is_some();
        if uses_async_auth {
            validate_custom_token_context(client_context_dict)?;
        }
        let mut context = PyCoreConnection::dict_to_client_context(client_context_dict)?;
        let mut auth_cancellation_callback = None;
        if let Some(provider) = token_provider {
            let factory = AsyncPythonEntraIdTokenFactory::new(py, provider)?;
            auth_cancellation_callback = Some(factory.connection_cancellation_callback(py)?);
            context.tds_authentication_method =
                TdsAuthenticationMethod::ActiveDirectoryTokenCredential;
            context.auth_method_map.insert(
                TdsAuthenticationMethod::ActiveDirectoryTokenCredential,
                Box::new(factory),
            );
        } else if let Some(config) = BuiltInEntraCredential::from_context(&mut context)? {
            let factory = AsyncPythonEntraIdTokenFactory::built_in(py, config)?;
            auth_cancellation_callback = Some(factory.connection_cancellation_callback(py)?);
            context
                .auth_method_map
                .insert(context.tds_authentication_method.clone(), Box::new(factory));
            uses_async_auth = true;
        }
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
        let future = pyo3_async_runtimes::tokio::future_into_py(
            py,
            with_optional_dispatch(
                async move {
                    let provider = TdsConnectionProvider {};
                    let client = provider
                        .create_client(context, &datasource, None)
                        .await
                        .map_err(|error| map_connect_error(error, uses_async_auth))?;

                    tracing::info!("PyAsyncConnection::connect: connection established");
                    let connection = Arc::new(LogicalConnection::new(client));
                    Python::attach(|py| {
                        Py::new(
                            py,
                            PyAsyncConnection {
                                session_state: connection.state.clone(),
                                tds_client: Some(connection),
                                tracing_dispatch: dispatch,
                                autocommit: Arc::new(AtomicBool::new(autocommit)),
                                default_query_timeout: 0,
                            },
                        )
                    })
                },
                future_dispatch,
            ),
        )?;
        if let Some(callback) = auth_cancellation_callback {
            callback.borrow(py).bind_future(&future)?;
            future.call_method1("add_done_callback", (callback,))?;
        }

        Ok(future)
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
    // TODO: Add an awaitable mode-change API. Enabling autocommit must commit
    // active work before changing mode and preserve the old mode on failure;
    // disabling it must defer begin until the next execute.
    #[getter]
    fn autocommit(&self) -> bool {
        self.autocommit.load(Ordering::Relaxed)
    }

    /// True after `close()` has been called. Cheap LBYL check; performs no I/O.
    #[getter]
    fn closed(&self) -> bool {
        self.tds_client.is_none()
            || matches!(
                self.session_state.ensure_open(),
                Err(ClaimError::Closing | ClaimError::Closed)
            )
    }

    /// Inverse of `.closed`, provided for sync-path parity with `PyCoreConnection`.
    fn is_connected(&self) -> bool {
        !self.closed()
    }

    fn __repr__(&self) -> &'static str {
        if self.closed() {
            "PyAsyncConnection(closed)"
        } else {
            "PyAsyncConnection(connected)"
        }
    }

    /// Close the connection, draining results before rolling back active work.
    /// Idempotent; concurrent callers await the same cleanup and outcome.
    /// Cleanup has one five-second budget, even if a caller is cancelled.
    /// Returns an awaitable that resolves to `None`.
    fn close<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let dispatch = self.tracing_dispatch.clone();
        let _guard = dispatch.as_ref().map(tracing::dispatcher::set_default);
        tracing::info!("PyAsyncConnection::close: initiating close");
        self.close_awaitable(py, CloseMode::Rollback, None)
    }

    /// Async context manager entry. Resolves to `self` with no I/O.
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        slf.borrow(py)
            .session_state
            .ensure_open()
            .map_err(map_claim_error)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(slf) })
    }

    /// Commit on clean exit or roll back on exceptional exit when autocommit is
    /// disabled, then always close. Cleanup never masks the block's exception.
    fn __aexit__<'py>(
        &mut self,
        py: Python<'py>,
        exc_type: &Bound<'_, PyAny>,
        exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let autocommit = self.autocommit.load(Ordering::Relaxed);
        let has_block_error = !exc_type.is_none();
        let mode = if !autocommit && !has_block_error {
            CloseMode::Commit
        } else {
            CloseMode::Rollback
        };
        self.close_awaitable(py, mode, has_block_error.then(|| exc_val.clone().unbind()))
    }

    /// Commit the current transaction. No-op if none is active.
    /// Drains pending results first; interrupted or failed I/O invalidates the connection.
    fn commit<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.transaction_awaitable(py, CloseMode::Commit)
    }

    /// Roll back the current transaction. No-op if none is active.
    /// Drains pending results first; interrupted or failed I/O invalidates the connection.
    fn rollback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.transaction_awaitable(py, CloseMode::Rollback)
    }

    /// Create an async cursor. Sync per DB-API 2.0. Raises `InterfaceError` if the
    /// connection is closed. Additional cursors are allowed; all share the same
    /// TDS session and serialize on the same async mutex.
    fn cursor(&self) -> PyResult<PyAsyncCursor> {
        self.session_state.ensure_open().map_err(map_claim_error)?;
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| InterfaceError::new_err("Connection is closed"))?
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

impl PyAsyncConnection {
    fn transaction_awaitable<'py>(
        &self,
        py: Python<'py>,
        mode: CloseMode,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| InterfaceError::new_err("Connection is closed"))?
            .clone();
        let state = self.session_state.clone();
        let (operation_id, previous) = state
            .claim_connection_operation()
            .map_err(map_claim_error)?;
        let future_state = state.clone();
        let dispatch = self.tracing_dispatch.clone();
        let future = async move {
            let mut guard = ConnectionOperationGuard::new(future_state, operation_id);
            let mut client = client
                .lock_operation(operation_id)
                .await
                .map_err(map_claim_error)?;
            let result = async {
                client.close_query().await?;
                if client.has_active_transaction() {
                    match mode {
                        CloseMode::Commit => client.commit_transaction(None, None).await?,
                        CloseMode::Rollback => client.rollback_transaction(None, None).await?,
                    }
                }
                Ok::<(), TdsError>(())
            }
            .await;
            let result = result.map_err(|error| {
                crate::async_errors::map_tds_error(
                    "Transaction completion failed",
                    error,
                    client.take_info_messages(),
                )
            });
            drop(client);
            if result.is_ok() {
                guard.complete();
            }
            result?;
            Python::attach(|py| Ok(py.None()))
        };
        match pyo3_async_runtimes::tokio::future_into_py(
            py,
            state
                .clone()
                .run_tracked(with_optional_dispatch(future, dispatch)),
        ) {
            Ok(awaitable) => Ok(awaitable),
            Err(error) => {
                state.restore_connection_operation(operation_id, previous);
                Err(error)
            }
        }
    }

    fn close_awaitable<'py>(
        &self,
        py: Python<'py>,
        mode: CloseMode,
        block_error: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self
            .tds_client
            .as_ref()
            .ok_or_else(|| InterfaceError::new_err("Connection is closed"))?;
        // Start before constructing the awaitable: construction failure cannot strand cleanup.
        let receiver = client.start_close(mode, CLOSE_TIMEOUT, self.tracing_dispatch.clone());
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let result = LogicalConnection::wait_closed(receiver).await;
            if let Some(block_error) = block_error {
                if result.is_err() {
                    tracing::warn!("Connection cleanup failed while preserving block exception");
                    Python::attach(|py| {
                        let error = block_error.bind(py);
                        if error.hasattr("add_note")? {
                            error.call_method1(
                                "add_note",
                                ("Connection cleanup failed; connection discarded",),
                            )?;
                        }
                        Ok::<(), PyErr>(())
                    })
                    .unwrap_or_else(|_| tracing::warn!("Could not annotate block exception"));
                }
            } else {
                result?;
            }
            Python::attach(|py| Ok(py.None()))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use mssql_tds::error::{Error, SqlErrorInfo, SqlServerDiagnostics, TimeoutErrorType};
    use pyo3::prelude::*;

    use super::{CLOSE_TIMEOUT, CloseMode, PyAsyncConnection, map_connect_error};
    use crate::async_errors::{DatabaseError, OperationalError};
    use crate::async_handle::LogicalConnection;

    #[test]
    fn cancelled_python_close_waiter_does_not_finish_a_concurrent_close_early() {
        use pyo3::types::{PyCFunction, PyDict};

        let runtime = pyo3_async_runtimes::tokio::get_runtime();
        let (client, transport) =
            mssql_tds::test_client_support::tds_client_from_tokens_capturing(Vec::new());
        let connection = std::sync::Arc::new(LogicalConnection::new(client));
        let state = connection.state.clone();
        let (operation, _) = state.claim_connection_operation().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let worker = connection.clone();
        let (result, _) = state
            .spawn_task(async move {
                let _client = worker.lock_operation(operation).await.unwrap();
                started.send(()).unwrap();
                wait.await.unwrap();
            })
            .unwrap();
        runtime.block_on(ready).unwrap();
        let wrapper = PyAsyncConnection {
            tds_client: Some(connection),
            tracing_dispatch: None,
            autocommit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            session_state: state.clone(),
            default_query_timeout: 0,
        };
        Python::attach(|py| {
            let globals = PyDict::new(py);
            globals
                .set_item("conn", Py::new(py, wrapper).unwrap())
                .unwrap();
            let release = std::sync::Mutex::new(Some(release));
            let release = PyCFunction::new_closure(py, None, None, move |_, _| {
                release.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok::<(), PyErr>(())
            })
            .unwrap();
            globals.set_item("release", release).unwrap();
            py.run(
                cr"import asyncio
async def run():
    first = conn.close()
    second = conn.close()
    assert not first.done()
    assert not second.done()
    first.cancel()
    try:
        await first
    except asyncio.CancelledError:
        pass
    else:
        raise AssertionError('close caller cancellation was not preserved')
    assert not second.done()
    release()
    await asyncio.wait_for(second, 5)
    assert conn.closed
    assert await conn.close() is None
asyncio.run(run())",
                Some(&globals),
                None,
            )
            .unwrap();
        });
        runtime.block_on(result).unwrap();
        assert_eq!(std::sync::Arc::strong_count(&transport), 1);
        assert!(transport.lock().unwrap().is_empty());
    }

    #[test]
    fn context_exit_after_handle_revocation_closes_without_io() {
        let (client, transport) =
            mssql_tds::test_client_support::tds_client_from_tokens_capturing(Vec::new());
        let connection = std::sync::Arc::new(LogicalConnection::new(client));
        drop(
            pyo3_async_runtimes::tokio::get_runtime()
                .block_on(connection.take_for_close())
                .unwrap(),
        );
        let state = connection.state.clone();
        let wrapper = PyAsyncConnection {
            tds_client: Some(connection),
            tracing_dispatch: None,
            autocommit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            session_state: state.clone(),
            default_query_timeout: 0,
        };
        Python::attach(|py| {
            let globals = pyo3::types::PyDict::new(py);
            globals
                .set_item("conn", Py::new(py, wrapper).unwrap())
                .unwrap();
            py.run(
                cr"import asyncio
async def run():
    assert await conn.__aexit__(None, None, None) is None
    assert conn.closed
    assert await conn.close() is None
asyncio.run(run())",
                Some(&globals),
                None,
            )
            .unwrap();
        });
        assert_eq!(
            state.lifecycle(),
            crate::async_session::ConnectionLifecycle::Closed
        );
        assert_eq!(std::sync::Arc::strong_count(&transport), 1);
        assert!(transport.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn close_after_handle_revocation_is_a_noop() {
        let (client, transport) =
            mssql_tds::test_client_support::tds_client_from_tokens_capturing(Vec::new());
        let connection = std::sync::Arc::new(LogicalConnection::new(client));
        drop(connection.take_for_close().await.unwrap());

        LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Rollback,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap();
        LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Commit,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap();
        assert_eq!(std::sync::Arc::strong_count(&transport), 1);
        assert!(transport.lock().unwrap().is_empty());
        assert!(connection.take_for_close().await.is_none());
    }

    #[test]
    fn connect_transport_errors_are_independent_of_auth_mode() {
        for uses_async_auth in [false, true] {
            for error in [
                Error::ConnectionError("connection refused".into()),
                Error::TlsHandshakeError {
                    source: Box::new(io::Error::other("certificate verification failed")),
                    expected_host: "server".into(),
                },
                Error::TimeoutError(TimeoutErrorType::String("login timed out".into())),
            ] {
                let error = map_connect_error(error, uses_async_auth);
                Python::attach(|py| assert!(error.is_instance_of::<OperationalError>(py)));
                assert!(
                    error
                        .to_string()
                        .contains("Failed to connect to SQL Server")
                );
            }
        }
    }

    #[test]
    fn connect_login_errors_preserve_diagnostics_for_all_auth_modes() {
        for uses_async_auth in [false, true] {
            let error = map_connect_error(
                Error::SqlServerError {
                    diagnostics: SqlServerDiagnostics::new(
                        vec![SqlErrorInfo {
                            message: "Login failed".into(),
                            state: 1,
                            class: 14,
                            number: 18456,
                            server_name: Some("server".into()),
                            proc_name: None,
                            line_number: None,
                            ..Default::default()
                        }],
                        Vec::new(),
                    ),
                },
                uses_async_auth,
            );
            Python::attach(|py| {
                assert!(error.is_instance_of::<DatabaseError>(py));
                assert_eq!(
                    error
                        .value(py)
                        .getattr("sql_errors")
                        .unwrap()
                        .get_item(0)
                        .unwrap()
                        .get_item("number")
                        .unwrap()
                        .extract::<i32>()
                        .unwrap(),
                    18456
                );
            });
        }
    }

    #[test]
    fn only_async_auth_connect_cancellation_raises_cancelled_error() {
        Python::attach(|py| {
            let cancelled_error = py
                .import("asyncio")
                .unwrap()
                .getattr("CancelledError")
                .unwrap();
            for uses_async_auth in [false, true] {
                let error = map_connect_error(
                    Error::OperationCancelledError("cancelled".into()),
                    uses_async_auth,
                );
                assert_eq!(error.is_instance(py, &cancelled_error), uses_async_auth);
                assert_eq!(
                    error.is_instance_of::<OperationalError>(py),
                    !uses_async_auth
                );
            }
        });
    }

    #[test]
    fn provider_connect_cancellation_logs_at_debug() {
        use std::sync::{Arc, Mutex};

        use tracing_subscriber::prelude::*;

        struct EventLevels(Arc<Mutex<Vec<tracing::Level>>>);

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLevels {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _context: tracing_subscriber::layer::Context<'_, S>,
            ) {
                self.0.lock().unwrap().push(*event.metadata().level());
            }
        }

        let levels = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(EventLevels(levels.clone()));
        tracing::subscriber::with_default(subscriber, || {
            Python::attach(|py| {
                let cancelled_error = py
                    .import("asyncio")
                    .unwrap()
                    .getattr("CancelledError")
                    .unwrap();
                for uses_async_auth in [false, true] {
                    let error =
                        map_connect_error(Error::TokenAcquisitionCancelled, uses_async_auth);
                    assert!(error.is_instance(py, &cancelled_error));
                }
            });
        });
        assert_eq!(
            *levels.lock().unwrap(),
            vec![tracing::Level::DEBUG, tracing::Level::DEBUG]
        );
    }
}

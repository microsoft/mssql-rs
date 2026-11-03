// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Loop-bound adapter for asynchronous Entra credentials.
//!
//! The provider's `get_token(scope)` returns an awaitable whose result exposes
//! a non-empty JWT string in `token`. Acquisition runs on the captured asyncio
//! loop; dropping the Rust operation schedules cancellation there, and a done
//! callback observes the outcome even after the caller has stopped waiting.
//! Custom providers remain caller-owned. Built-in credentials are acquired and
//! closed on the same loop for each token attempt, before the token reaches TDS.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;
use mssql_tds::security::SecurityError;
use pyo3::exceptions::{
    PyAttributeError, PyRuntimeError, PyTimeoutError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3_async_runtimes::TaskLocals;
use tokio::sync::oneshot;

use crate::async_entra_credentials::BuiltInEntraCredential;
use crate::async_errors::InterfaceError;

const CREDENTIAL_CLOSE_TIMEOUT_SECS: f64 = 5.0;

#[derive(Clone)]
enum TokenProvider {
    Custom(Arc<Py<PyAny>>),
    BuiltIn(Arc<BuiltInEntraCredential>),
}

#[derive(Clone)]
pub(crate) struct AsyncPythonEntraIdTokenFactory {
    provider: TokenProvider,
    locals: TaskLocals,
    dispatch: tracing::Dispatch,
    connect_cancelled: Arc<AtomicBool>,
}

impl AsyncPythonEntraIdTokenFactory {
    pub(crate) fn new(py: Python<'_>, provider: Py<PyAny>) -> PyResult<Self> {
        let get_token = provider.bind(py).getattr("get_token").map_err(|_| {
            InterfaceError::new_err("token_provider must have a callable get_token(scope) method")
        })?;
        if !get_token.is_callable() {
            return Err(InterfaceError::new_err(
                "token_provider must have a callable get_token(scope) method",
            ));
        }
        Ok(Self {
            provider: TokenProvider::Custom(Arc::new(provider)),
            locals: pyo3_async_runtimes::tokio::get_current_locals(py)?,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            connect_cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn built_in(py: Python<'_>, config: BuiltInEntraCredential) -> PyResult<Self> {
        BuiltInEntraCredential::check_dependencies(py)?;
        Ok(Self {
            provider: TokenProvider::BuiltIn(Arc::new(config)),
            locals: pyo3_async_runtimes::tokio::get_current_locals(py)?,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            connect_cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn connection_cancellation_callback(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Py::new(
            py,
            ConnectCancellation {
                cancelled: self.connect_cancelled.clone(),
            },
        )
        .map(Py::into_any)
    }

    async fn acquire_token(&self, spn: &str, sts_url: &str) -> PyResult<Vec<u8>> {
        let scope = token_scope(spn)?;
        let (sender, receiver) = oneshot::channel();
        let mut cancellation = Python::attach(|py| {
            let completion = Py::new(
                py,
                TokenCompletion {
                    sender: Some(sender),
                    owned_provider: None,
                    task: None,
                    dispatch: self.dispatch.clone(),
                },
            )?;
            let request = Py::new(
                py,
                TokenRequest {
                    provider: self.provider.clone(),
                    scope,
                    sts_url: sts_url.to_owned(),
                    completion,
                    cancelled: false,
                },
            )?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("context", self.locals.context(py))?;
            self.locals.event_loop(py).call_method(
                "call_soon_threadsafe",
                (request.bind(py).getattr("start")?,),
                Some(&kwargs),
            )?;
            Ok::<_, PyErr>(AcquisitionCancellation {
                request,
                locals: self.locals.clone(),
                dispatch: self.dispatch.clone(),
                completed: false,
            })
        })?;

        let result = receiver.await.map_err(|_| {
            PyRuntimeError::new_err("Async Entra token dispatcher ended without a result")
        })?;
        cancellation.completed = true;
        result
    }
}

#[async_trait]
impl EntraIdTokenFactory for AsyncPythonEntraIdTokenFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        // TODO: In mssql-tds TdsClient::reconnect, stop retrying terminal auth
        // failures and propagate OperationCancelledError without wrapping it in
        // SessionRecoveryFailed, so recovery preserves initial-login semantics.
        let result = self.acquire_token(&spn, &sts_url).await;
        // The caller loop can observe cancellation before Tokio drops this future.
        if self.connect_cancelled.load(Ordering::Acquire) {
            return Err(Error::OperationCancelledError(
                "Async Entra token acquisition cancelled".into(),
            ));
        }
        result.map_err(|error| {
            let cancelled = Python::attach(|py| {
                py.import("asyncio")
                    .and_then(|module| module.getattr("CancelledError"))
                    .is_ok_and(|class| error.is_instance(py, &class))
            });
            if cancelled {
                Error::OperationCancelledError("Async Entra token acquisition cancelled".into())
            } else {
                // Provider messages can contain credentials or tokens.
                Error::Security(SecurityError::AuthenticationDenied(
                    "Async Entra token acquisition failed".into(),
                ))
            }
        })
    }
}

#[pyclass]
struct ConnectCancellation {
    cancelled: Arc<AtomicBool>,
}

#[pymethods]
impl ConnectCancellation {
    fn __call__(&self, future: &Bound<'_, PyAny>) -> PyResult<()> {
        if future.call_method0("cancelled")?.is_truthy()? {
            self.cancelled.store(true, Ordering::Release);
        }
        Ok(())
    }
}

fn token_scope(spn: &str) -> PyResult<String> {
    let resource = spn.trim_end_matches('/');
    if resource.is_empty()
        || resource
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(PyValueError::new_err(
            "Entra token SPN must be non-empty and contain no whitespace or control characters",
        ));
    }
    Ok(if resource.ends_with("/.default") {
        resource.to_owned()
    } else {
        format!("{resource}/.default")
    })
}

fn schedule_awaitable<'py>(awaitable: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let py = awaitable.py();
    let result = (|| {
        let asyncio = py.import("asyncio")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("loop", asyncio.call_method0("get_running_loop")?)?;
        asyncio.call_method("ensure_future", (awaitable,), Some(&kwargs))
    })();
    if result.is_err() {
        let discard = (|| {
            let coroutine = py.import("types")?.getattr("CoroutineType")?;
            if awaitable.is_instance(&coroutine)? {
                awaitable.call_method0("close")?;
            }
            Ok::<_, PyErr>(())
        })();
        if discard.is_err() {
            tracing::warn!("Failed to discard an unscheduled async Entra coroutine");
        }
    }
    result
}

#[pyclass]
struct TokenRequest {
    provider: TokenProvider,
    scope: String,
    sts_url: String,
    completion: Py<TokenCompletion>,
    cancelled: bool,
}

#[pymethods]
impl TokenRequest {
    fn start(&mut self, py: Python<'_>) {
        let dispatch = self.completion.borrow(py).dispatch.clone();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        if self.cancelled
            || self
                .completion
                .borrow(py)
                .sender
                .as_ref()
                .is_none_or(|sender| sender.is_closed())
        {
            return;
        }
        tracing::debug!(
            "Starting async Entra token acquisition (driver-owned={})",
            matches!(self.provider, TokenProvider::BuiltIn(_))
        );
        let result = (|| {
            let provider = match &self.provider {
                TokenProvider::Custom(provider) => provider.clone_ref(py),
                TokenProvider::BuiltIn(config) => {
                    let provider = config.create(py, &self.scope, &self.sts_url)?;
                    self.completion.borrow_mut(py).owned_provider = Some(provider.clone_ref(py));
                    provider
                }
            };
            let awaitable = provider
                .bind(py)
                .call_method1("get_token", (&self.scope,))?;
            let task = schedule_awaitable(&awaitable)?;
            self.completion.borrow_mut(py).task = Some(task.clone().unbind());
            task.call_method1("add_done_callback", (self.completion.bind(py),))?;
            Ok::<_, PyErr>(())
        })();
        if let Err(error) = result {
            self.cancel(py);
            self.completion.borrow_mut(py).complete(py, Err(error));
        }
    }

    fn cancel(&mut self, py: Python<'_>) {
        let dispatch = self.completion.borrow(py).dispatch.clone();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        if self.cancelled {
            return;
        }
        self.cancelled = true;
        tracing::debug!("Cancelling async Entra token acquisition");
        if let Some(task) = self.completion.borrow(py).task.as_ref()
            && task.call_method0(py, "cancel").is_err()
        {
            tracing::warn!("Failed to cancel async Entra token acquisition task");
        }
    }
}

#[pyclass]
struct TokenCompletion {
    sender: Option<oneshot::Sender<PyResult<Vec<u8>>>>,
    owned_provider: Option<Py<PyAny>>,
    // asyncio keeps only weak references to tasks, including cancelled ones.
    task: Option<Py<PyAny>>,
    dispatch: tracing::Dispatch,
}

impl TokenCompletion {
    fn complete(&mut self, py: Python<'_>, result: PyResult<Vec<u8>>) {
        if let Some(provider) = self.owned_provider.take() {
            tracing::debug!("Closing driver-owned async Entra credential");
            let cleanup = (|| {
                let completion = Py::new(
                    py,
                    CredentialCloseCompletion {
                        sender: None,
                        result: None,
                        provider: None,
                        task: None,
                        timer: None,
                        dispatch: self.dispatch.clone(),
                    },
                )?;
                let close = provider.call_method0(py, "close")?;
                let task = schedule_awaitable(close.bind(py))?;
                task.call_method1("add_done_callback", (completion.bind(py),))?;
                completion.borrow_mut(py).task = Some(task.unbind());
                let timer = py
                    .import("asyncio")?
                    .call_method0("get_running_loop")?
                    .call_method1(
                        "call_later",
                        (
                            CREDENTIAL_CLOSE_TIMEOUT_SECS,
                            completion.bind(py).getattr("timeout")?,
                        ),
                    )?;
                completion.borrow_mut(py).timer = Some(timer.unbind());
                Ok::<_, PyErr>(completion)
            })();
            match cleanup {
                Ok(completion) => {
                    let mut completion = completion.borrow_mut(py);
                    completion.sender = self.sender.take();
                    completion.result = Some(result);
                    completion.provider = Some(provider);
                    return;
                }
                Err(error) => {
                    tracing::warn!("Failed to schedule driver-owned Entra credential cleanup");
                    self.send(result.and(Err(error)));
                    return;
                }
            }
        }
        self.send(result);
    }

    fn send(&mut self, result: PyResult<Vec<u8>>) {
        if let Some(sender) = self.sender.take() {
            // A dropped receiver means the native operation was cancelled.
            let _ = sender.send(result);
        }
    }
}

#[pymethods]
impl TokenCompletion {
    fn __call__(&mut self, task: &Bound<'_, PyAny>) {
        let dispatch = self.dispatch.clone();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        self.task.take();
        // Always retrieve the outcome, even after the Rust receiver is dropped.
        let result = task.call_method0("result").and_then(|value| {
            let py = value.py();
            let token = value
                .getattr("token")
                .map_err(|error| {
                    if error.is_instance_of::<PyAttributeError>(py) {
                        PyTypeError::new_err(
                            "Async token provider must return an object with a non-empty string token",
                        )
                    } else {
                        error
                    }
                })?
                .extract::<String>()
                .map_err(|_| {
                    PyTypeError::new_err(
                        "Async token provider must return an object with a non-empty string token",
                    )
                })?;
            if token.trim().is_empty() {
                return Err(PyValueError::new_err(
                    "Async token provider returned an empty token",
                ));
            }
            Ok(token.encode_utf16().flat_map(u16::to_le_bytes).collect())
        });
        if result.is_err() {
            tracing::debug!("Async Entra token acquisition failed or was cancelled");
        } else {
            tracing::debug!("Async Entra token acquisition completed");
        }
        self.complete(task.py(), result);
    }
}

#[pyclass]
struct CredentialCloseCompletion {
    sender: Option<oneshot::Sender<PyResult<Vec<u8>>>>,
    result: Option<PyResult<Vec<u8>>>,
    provider: Option<Py<PyAny>>,
    task: Option<Py<PyAny>>,
    timer: Option<Py<PyAny>>,
    dispatch: tracing::Dispatch,
}

impl CredentialCloseCompletion {
    fn finish(&mut self, closed: PyResult<()>) {
        if let Some(sender) = self.sender.take()
            && let Some(result) = self.result.take()
        {
            let _ = sender.send(result.and_then(|bytes| closed.map(|_| bytes)));
        }
    }
}

#[pymethods]
impl CredentialCloseCompletion {
    fn timeout(&mut self, py: Python<'_>) {
        let dispatch = self.dispatch.clone();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        self.timer.take();
        tracing::warn!(
            "Timed out closing driver-owned async Entra credential; requesting cancellation"
        );
        if let Some(task) = self.task.as_ref()
            && task.call_method0(py, "cancel").is_err()
        {
            tracing::warn!("Failed to cancel driver-owned Entra credential cleanup task");
        }
        // wait_for can exceed its timeout when close suppresses cancellation.
        // Keep observing the task, but never publish its token after the deadline.
        self.finish(Err(PyTimeoutError::new_err(
            "Timed out closing driver-owned async Entra credential",
        )));
    }

    fn __call__(&mut self, task: &Bound<'_, PyAny>) {
        let dispatch = self.dispatch.clone();
        let _guard = tracing::dispatcher::set_default(&dispatch);
        if let Some(timer) = self.timer.take()
            && timer.call_method0(task.py(), "cancel").is_err()
        {
            tracing::warn!("Failed to cancel async Entra credential cleanup timer");
        }
        self.task.take();
        let closed = task.call_method0("result").map(|_| ());
        if closed.is_err() {
            tracing::warn!("Failed to close driver-owned async Entra credential");
        } else {
            tracing::debug!("Driver-owned async Entra credential closed");
        }
        self.provider.take();
        self.finish(closed);
    }
}

struct AcquisitionCancellation {
    request: Py<TokenRequest>,
    locals: TaskLocals,
    dispatch: tracing::Dispatch,
    completed: bool,
}

impl Drop for AcquisitionCancellation {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let _guard = tracing::dispatcher::set_default(&self.dispatch);
        Python::attach(|py| {
            let result = self.request.bind(py).getattr("cancel").and_then(|cancel| {
                self.locals
                    .event_loop(py)
                    .call_method1("call_soon_threadsafe", (cancel,))
            });
            if result.is_err() {
                tracing::warn!(
                    "Failed to schedule async Entra token cancellation on the originating loop"
                );
            }
        });
    }
}

/// Internal bridge exerciser; no connection or Azure SDK dependency is required.
#[pyfunction]
pub(crate) fn _invoke_async_entra_id_token_factory<'py>(
    py: Python<'py>,
    provider: Py<PyAny>,
    spn: String,
) -> PyResult<Bound<'py, PyAny>> {
    let factory = AsyncPythonEntraIdTokenFactory::new(py, provider)?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let bytes = factory.acquire_token(&spn, "").await?;
        Python::attach(|py| Ok(PyBytes::new(py, &bytes).unbind()))
    })
}

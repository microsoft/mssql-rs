// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Loop-bound adapter for caller-owned asynchronous Entra credentials.
//!
//! The provider's `get_token(scope)` returns an awaitable whose result exposes
//! a non-empty JWT string in `token`. Acquisition runs on the captured asyncio
//! loop; dropping the Rust operation schedules cancellation there, and a done
//! callback observes the outcome even after the caller has stopped waiting.
//! Providers remain caller-owned. Built-in SDK credential selection is separate.

use std::sync::Arc;

use async_trait::async_trait;
use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;
use mssql_tds::security::SecurityError;
use pyo3::exceptions::{PyAttributeError, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3_async_runtimes::TaskLocals;
use tokio::sync::oneshot;

#[derive(Clone)]
pub(crate) struct AsyncPythonEntraIdTokenFactory {
    provider: Arc<Py<PyAny>>,
    locals: TaskLocals,
}

impl AsyncPythonEntraIdTokenFactory {
    pub(crate) fn new(py: Python<'_>, provider: Py<PyAny>) -> PyResult<Self> {
        let get_token = provider.bind(py).getattr("get_token").map_err(|error| {
            if error.is_instance_of::<PyAttributeError>(py) {
                PyTypeError::new_err("token_provider must have a callable get_token(scope) method")
            } else {
                error
            }
        })?;
        if !get_token.is_callable() {
            return Err(PyTypeError::new_err(
                "token_provider must have a callable get_token(scope) method",
            ));
        }
        Ok(Self {
            provider: Arc::new(provider),
            locals: pyo3_async_runtimes::tokio::get_current_locals(py)?,
        })
    }

    async fn acquire_token(&self, spn: &str) -> PyResult<Vec<u8>> {
        let scope = token_scope(spn)?;
        let (sender, receiver) = oneshot::channel();
        let mut cancellation = Python::attach(|py| {
            let completion = Py::new(
                py,
                TokenCompletion {
                    sender: Some(sender),
                },
            )?;
            let request = Py::new(
                py,
                TokenRequest {
                    provider: self.provider.clone_ref(py),
                    scope,
                    completion,
                    task: None,
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
        _sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        self.acquire_token(&spn).await.map_err(|error| {
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

#[pyclass]
struct TokenRequest {
    provider: Py<PyAny>,
    scope: String,
    completion: Py<TokenCompletion>,
    task: Option<Py<PyAny>>,
    cancelled: bool,
}

#[pymethods]
impl TokenRequest {
    fn start(&mut self, py: Python<'_>) {
        if self.cancelled {
            return;
        }
        let result = (|| {
            let awaitable = self
                .provider
                .bind(py)
                .call_method1("get_token", (&self.scope,))?;
            let asyncio = py.import("asyncio")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("loop", asyncio.call_method0("get_running_loop")?)?;
            let task = asyncio.call_method("ensure_future", (awaitable,), Some(&kwargs))?;
            self.task = Some(task.clone().unbind());
            task.call_method1("add_done_callback", (self.completion.bind(py),))?;
            Ok::<_, PyErr>(())
        })();
        if let Err(error) = result {
            self.cancel(py);
            self.completion.borrow_mut(py).complete(Err(error));
        }
    }

    fn cancel(&mut self, py: Python<'_>) {
        self.cancelled = true;
        if let Some(task) = self.task.take()
            && task.call_method0(py, "cancel").is_err()
        {
            tracing::warn!("Failed to cancel async Entra token acquisition task");
        }
    }
}

#[pyclass]
struct TokenCompletion {
    sender: Option<oneshot::Sender<PyResult<Vec<u8>>>>,
}

impl TokenCompletion {
    fn complete(&mut self, result: PyResult<Vec<u8>>) {
        if let Some(sender) = self.sender.take() {
            // A dropped receiver means the native operation was cancelled.
            let _ = sender.send(result);
        }
    }
}

#[pymethods]
impl TokenCompletion {
    fn __call__(&mut self, task: &Bound<'_, PyAny>) {
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
        self.complete(result);
    }
}

struct AcquisitionCancellation {
    request: Py<TokenRequest>,
    locals: TaskLocals,
    completed: bool,
}

impl Drop for AcquisitionCancellation {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
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
        let bytes = factory.acquire_token(&spn).await?;
        Python::attach(|py| Ok(PyBytes::new(py, &bytes).unbind()))
    })
}

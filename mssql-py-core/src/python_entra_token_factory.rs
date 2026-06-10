// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Adapter that bridges mssql-tds' [`EntraIdTokenFactory`] trait to a Python
//! callable. Used by py-core to let mssql-python provide Entra ID tokens during
//! FedAuth handshake (e.g. `ActiveDirectoryServicePrincipal`).
//!
//! GIL scope is kept narrow: the Python callable is invoked inside
//! `tokio::task::spawn_blocking` so the Tokio runtime is not stalled while the
//! GIL is held and azure-identity does its synchronous network I/O to the
//! token endpoint.

use std::sync::Arc;

use async_trait::async_trait;
use pyo3::prelude::*;

use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;

/// Token factory that delegates to a Python callable.
///
/// The callable is invoked with `(spn, sts_url, auth_method_name)` (all `str`)
/// and must return `bytes` (the JWT in the format expected by the FedAuth wire
/// protocol — UTF-16LE encoded for SQL Server).
#[derive(Clone)]
pub(crate) struct PythonEntraIdTokenFactory {
    callable: Arc<Py<PyAny>>,
}

impl PythonEntraIdTokenFactory {
    pub(crate) fn new(callable: Py<PyAny>) -> Self {
        Self {
            callable: Arc::new(callable),
        }
    }
}

#[async_trait]
impl EntraIdTokenFactory for PythonEntraIdTokenFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        let callable = Arc::clone(&self.callable);
        let auth_method_name = auth_method_to_str(&auth_method).to_string();

        tokio::task::spawn_blocking(move || -> TdsResult<Vec<u8>> {
            Python::attach(|py| {
                let bound = callable.bind(py);
                let result = bound
                    .call1((spn.as_str(), sts_url.as_str(), auth_method_name.as_str()))
                    .map_err(|e| {
                        Error::ConnectionError(format!("Entra ID token callback raised: {e}"))
                    })?;
                result.extract::<Vec<u8>>().map_err(|e| {
                    Error::ConnectionError(format!(
                        "Entra ID token callback returned non-bytes value: {e}"
                    ))
                })
            })
        })
        .await
        .map_err(|e| Error::ConnectionError(format!("Token callback task panicked: {e}")))?
    }
}

fn auth_method_to_str(auth_method: &TdsAuthenticationMethod) -> &'static str {
    match auth_method {
        TdsAuthenticationMethod::ActiveDirectoryServicePrincipal => {
            "activedirectoryserviceprincipal"
        }
        TdsAuthenticationMethod::ActiveDirectoryPassword => "activedirectorypassword",
        TdsAuthenticationMethod::ActiveDirectoryInteractive => "activedirectoryinteractive",
        TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow => "activedirectorydevicecodeflow",
        TdsAuthenticationMethod::ActiveDirectoryManagedIdentity => "activedirectorymanagedidentity",
        TdsAuthenticationMethod::ActiveDirectoryMSI => "activedirectorymsi",
        TdsAuthenticationMethod::ActiveDirectoryDefault => "activedirectorydefault",
        TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity => {
            "activedirectoryworkloadidentity"
        }
        TdsAuthenticationMethod::ActiveDirectoryIntegrated => "activedirectoryintegrated",
        TdsAuthenticationMethod::Password => "password",
        TdsAuthenticationMethod::SSPI => "sspi",
        TdsAuthenticationMethod::AccessToken => "accesstoken",
    }
}

/// Test-only hook to drive `PythonEntraIdTokenFactory::create_token` from
/// Python. Builds a factory wrapping `callable`, spawns a fresh tokio
/// runtime, and returns the produced bytes (or raises a `RuntimeError`
/// with the mapped message).
///
/// Intentionally exposed under an underscore-prefixed name so it is not
/// part of the public py-core API. The existing Python tests use it to
/// cover the success path, Python-exception mapping, and non-bytes-return
/// mapping that the dict-key plumbing tests do not reach.
#[pyfunction]
#[pyo3(name = "_invoke_entra_id_token_factory")]
pub fn invoke_entra_id_token_factory(
    py: Python<'_>,
    callable: Py<PyAny>,
    spn: String,
    sts_url: String,
) -> PyResult<Vec<u8>> {
    use pyo3::exceptions::PyRuntimeError;
    let factory = PythonEntraIdTokenFactory::new(callable);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PyRuntimeError::new_err(format!("tokio runtime build failed: {e}")))?;
    // Release the GIL while driving the runtime: `create_token` uses
    // `spawn_blocking` + `Python::attach`, which would deadlock if the GIL
    // were held by this thread.
    let result = py.detach(|| {
        rt.block_on(factory.create_token(
            spn,
            sts_url,
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
        ))
    });
    result.map_err(|e| PyRuntimeError::new_err(format!("{e:?}")))
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::CancelHandle;
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;
use mssql_tds::security::SecurityError;
use tokio::time::Instant;

use crate::async_entra_token_factory::{AsyncPythonEntraIdTokenFactory, token_error};

use super::connection::{
    AuthBinding, AuthMetadata, ConnectSpec, ConnectionFactory, OpenedConnection, TokenExpiry,
};
use super::error::{OpenError, PoolError, PoolResult};

/// One physical context's callback. Never put this on a per-key manager.
#[derive(Clone)]
pub(crate) struct RecoveryAuth {
    binding: AuthBinding,
    tenant: Arc<str>,
    active: Arc<Mutex<Weak<ActiveAuth>>>,
}

impl RecoveryAuth {
    pub(super) fn owns(&self, operation: &OperationAuth) -> bool {
        Arc::ptr_eq(&self.active, &operation.retained.active)
    }

    /// Binding and tenant must come from verified preparation, not token claims.
    pub(crate) fn new(binding: AuthBinding, tenant: String) -> PoolResult<Self> {
        if tenant.is_empty()
            || !tenant
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.'))
            || ["common", "organizations", "consumers"]
                .iter()
                .any(|value| tenant.eq_ignore_ascii_case(value))
        {
            return Err(PoolError::InvalidRequest {
                field: "tenant",
                reason: "a verified concrete Azure Public tenant is required",
            });
        }
        Ok(Self {
            binding,
            tenant: tenant.into(),
            active: Arc::default(),
        })
    }

    /// The caller constructs the adapter on its current running Python loop.
    pub(crate) fn activate(
        &self,
        factory: AsyncPythonEntraIdTokenFactory,
        current_binding: impl Fn() -> PoolResult<AuthBinding> + Send + Sync + 'static,
        margin: Duration,
    ) -> PoolResult<OperationAuth> {
        self.verify_binding(&current_binding()?)?;
        let mut active = self.active.lock().unwrap();
        if active.upgrade().is_some() {
            return Err(PoolError::InvalidRequest {
                field: "authentication",
                reason: "a credential operation is already active",
            });
        }
        let inner = Arc::new(ActiveAuth {
            factory,
            enabled: AtomicBool::new(true),
            login: Mutex::new(None),
            margin,
            current_binding: Box::new(current_binding),
        });
        *active = Arc::downgrade(&inner);
        Ok(OperationAuth {
            retained: self.clone(),
            inner,
        })
    }

    fn verify_binding(&self, current: &AuthBinding) -> PoolResult<()> {
        if *current != self.binding {
            return Err(PoolError::IdentityMismatch);
        }
        Ok(())
    }

    fn verify_active_binding(&self, active: &ActiveAuth) -> PoolResult<()> {
        self.verify_binding(&(active.current_binding)()?)
    }
}

struct ActiveAuth {
    factory: AsyncPythonEntraIdTokenFactory,
    enabled: AtomicBool,
    login: Mutex<Option<LoginExpiry>>,
    margin: Duration,
    current_binding: Box<dyn Fn() -> PoolResult<AuthBinding> + Send + Sync>,
}

struct LoginExpiry {
    utc: DateTime<Utc>,
    deadline: Instant,
}

impl LoginExpiry {
    fn new(utc: DateTime<Utc>, margin: Duration) -> PoolResult<Self> {
        let now = Instant::now();
        let remaining = (utc - Utc::now())
            .to_std()
            .map_err(|_| PoolError::TokenExpired)?;
        let deadline = now.checked_add(remaining).ok_or(PoolError::TokenExpired)?;
        let expiry = Self { utc, deadline };
        expiry.verify(margin)?;
        Ok(expiry)
    }

    fn verify(&self, margin: Duration) -> PoolResult<()> {
        if !(self.utc - Utc::now())
            .to_std()
            .is_ok_and(|left| left > margin)
            || self.deadline.saturating_duration_since(Instant::now()) <= margin
        {
            return Err(PoolError::TokenExpired);
        }
        Ok(())
    }
}

/// Strong Python ownership ends when this operation scope is dropped.
pub(crate) struct OperationAuth {
    retained: RecoveryAuth,
    inner: Arc<ActiveAuth>,
}

impl OperationAuth {
    #[cfg(test)]
    pub(super) fn record_test_login(&self, expiry: DateTime<Utc>) -> PoolResult<()> {
        *self.inner.login.lock().unwrap() = Some(LoginExpiry::new(expiry, self.inner.margin)?);
        Ok(())
    }

    pub(super) fn check_binding(&self) -> PoolResult<()> {
        self.retained.verify_active_binding(&self.inner)
    }

    /// Call only after successful login/recovery, before publishing its result.
    /// Re-read the trusted binding epoch; a miss-only identity check is insufficient.
    /// Expired metadata disallows reuse, not delivery of already-completed application work.
    pub(crate) fn complete(self) -> PoolResult<Option<AuthMetadata>> {
        self.metadata()
    }

    fn metadata(&self) -> PoolResult<Option<AuthMetadata>> {
        self.check_binding()?;
        let login = self.inner.login.lock().unwrap();
        login
            .as_ref()
            .map(|login| {
                login.verify(self.inner.margin)?;
                Ok(AuthMetadata {
                    binding: self.retained.binding.clone(),
                    expiry: TokenExpiry::Known(login.utc),
                })
            })
            .transpose()
    }
}

#[async_trait]
impl ConnectionFactory for OperationAuth {
    async fn open(
        &self,
        spec: &ConnectSpec,
        cancel: &CancelHandle,
    ) -> Result<OpenedConnection, OpenError> {
        self.retained
            .verify_binding(spec.expected_auth())
            .map_err(|error| OpenError::Provider(Box::new(error)))?;
        self.check_binding()
            .map_err(|error| OpenError::Provider(Box::new(error)))?;
        let mut context = spec.context().clone();
        if !context.auth_method_map.is_empty()
            || context.access_token.is_some()
            || !matches!(
                context.tds_authentication_method,
                TdsAuthenticationMethod::ActiveDirectoryDefault
                    | TdsAuthenticationMethod::ActiveDirectoryMSI
                    | TdsAuthenticationMethod::ActiveDirectoryManagedIdentity
                    | TdsAuthenticationMethod::ActiveDirectoryServicePrincipal
            )
        {
            return Err(OpenError::Tds(Box::new(Error::Security(
                SecurityError::AuthenticationDenied(
                    "Unsupported operation-scoped Entra configuration".into(),
                ),
            ))));
        }
        context.auth_method_map.insert(
            context.tds_authentication_method.clone(),
            Box::new(self.retained.clone()),
        );
        let client = TdsConnectionProvider {}
            .create_client(context, &spec.context().data_source, Some(cancel))
            .await
            .map_err(|error| OpenError::Tds(Box::new(error)))?;
        let auth = self
            .metadata()
            .map_err(|error| OpenError::Provider(Box::new(error)))?
            .ok_or_else(|| OpenError::Tds(Box::new(inactive())))?;
        Ok(OpenedConnection::with_recovery(
            client,
            auth,
            self.retained.clone(),
        ))
    }
}

impl Drop for OperationAuth {
    fn drop(&mut self) {
        self.inner.enabled.store(false, Ordering::Release);
        *self.retained.active.lock().unwrap() = Weak::new();
    }
}

fn inactive() -> Error {
    Error::Security(SecurityError::AuthenticationDenied(
        "No active Entra credential operation".into(),
    ))
}

#[async_trait]
impl EntraIdTokenFactory for RecoveryAuth {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        _method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        let active = self.active.lock().unwrap().upgrade().ok_or_else(inactive)?;
        if !active.enabled.load(Ordering::Acquire) {
            return Err(inactive());
        }
        self.verify_active_binding(&active)
            .map_err(|_| binding_changed())?;
        // A routed/retried login must not inherit a previous attempt's expiry.
        active.login.lock().unwrap().take();
        let token = active
            .factory
            .acquire_login_token(&spn, &sts_url, &self.tenant)
            .await
            .map_err(token_error)?;
        if !active.enabled.load(Ordering::Acquire) {
            return Err(inactive());
        }
        self.verify_active_binding(&active)
            .map_err(|_| binding_changed())?;
        let expiry = LoginExpiry::new(token.expires_on.ok_or_else(inactive)?, active.margin)
            .map_err(|_| {
                Error::Security(SecurityError::AuthenticationDenied(
                    "Entra token is expired or inside the refresh margin".into(),
                ))
            })?;
        *active.login.lock().unwrap() = Some(expiry);
        Ok(token.bytes)
    }
}

fn binding_changed() -> Error {
    Error::Security(SecurityError::AuthenticationDenied(
        "Entra credential binding changed".into(),
    ))
}

/// Exercises the production callback/operation seam without a SQL endpoint.
#[cfg(debug_assertions)]
#[pyo3::pyclass(name = "_EntraRecoveryAuth")]
pub(crate) struct RecoveryAuthHarness {
    retained: RecoveryAuth,
    binding: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(debug_assertions)]
fn harness_binding(value: u64) -> AuthBinding {
    AuthBinding::new(value.to_le_bytes().to_vec().into_boxed_slice()).expect("nonempty binding")
}

#[cfg(debug_assertions)]
#[pyo3::pymethods]
impl RecoveryAuthHarness {
    #[new]
    fn new() -> pyo3::PyResult<Self> {
        Ok(Self {
            retained: RecoveryAuth::new(harness_binding(1), "tenant".into())
                .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("Invalid test binding"))?,
            binding: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        })
    }

    fn set_binding(&self, binding: u64) {
        self.binding.store(binding, Ordering::Release);
    }

    fn collect_python_references(&self, py: pyo3::Python<'_>) {
        py.detach(|| pyo3::Python::attach(|_| ()));
    }

    #[pyo3(signature = (provider, spn, sts_url, margin_secs=0))]
    fn invoke<'py>(
        &self,
        py: pyo3::Python<'py>,
        provider: pyo3::Py<pyo3::PyAny>,
        spn: String,
        sts_url: String,
        margin_secs: u64,
    ) -> pyo3::PyResult<pyo3::Bound<'py, pyo3::PyAny>> {
        let binding = self.binding.clone();
        let factory = AsyncPythonEntraIdTokenFactory::new(py, provider)?;
        let operation = py
            .detach(|| {
                self.retained.activate(
                    factory,
                    move || Ok(harness_binding(binding.load(Ordering::Acquire))),
                    Duration::from_secs(margin_secs),
                )
            })
            .map_err(|_| {
                pyo3::exceptions::PyRuntimeError::new_err(
                    "Credential binding is inactive or changed",
                )
            })?;
        let retained = self.retained.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let bytes = retained
                .create_token(
                    spn,
                    sts_url,
                    TdsAuthenticationMethod::ActiveDirectoryDefault,
                )
                .await
                .map_err(|_| {
                    pyo3::exceptions::PyRuntimeError::new_err("Credential recovery failed")
                })?;
            let metadata = operation
                .complete()
                .map_err(|_| {
                    pyo3::exceptions::PyRuntimeError::new_err(
                        "Credential binding or expiry changed",
                    )
                })?
                .ok_or_else(|| {
                    pyo3::exceptions::PyRuntimeError::new_err("No successful login metadata")
                })?;
            let TokenExpiry::Known(expiry) = metadata.expiry else {
                unreachable!()
            };
            pyo3::Python::attach(|py| {
                Ok((
                    pyo3::types::PyBytes::new(py, &bytes).unbind(),
                    expiry.timestamp(),
                ))
            })
        })
    }

    fn invoke_inactive<'py>(
        &self,
        py: pyo3::Python<'py>,
    ) -> pyo3::PyResult<pyo3::Bound<'py, pyo3::PyAny>> {
        let retained = self.retained.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            retained
                .create_token(
                    String::new(),
                    String::new(),
                    TdsAuthenticationMethod::ActiveDirectoryDefault,
                )
                .await
                .map(|_| ())
                .map_err(|_| {
                    pyo3::exceptions::PyRuntimeError::new_err(
                        "No active Entra credential operation",
                    )
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inactive_callback_is_terminal_and_retains_no_python_objects() {
        let auth = RecoveryAuth::new(
            AuthBinding::new(vec![1].into_boxed_slice()).unwrap(),
            "tenant".into(),
        )
        .unwrap();
        assert!(matches!(
            auth.create_token(
                "resource".into(),
                "authority".into(),
                TdsAuthenticationMethod::ActiveDirectoryDefault
            )
            .await,
            Err(Error::Security(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_checks_utc_margin_and_monotonic_elapsed_time() {
        assert!(matches!(
            LoginExpiry::new(Utc::now(), Duration::ZERO),
            Err(PoolError::TokenExpired)
        ));
        assert!(matches!(
            LoginExpiry::new(
                Utc::now() + chrono::Duration::seconds(10),
                Duration::from_secs(30)
            ),
            Err(PoolError::TokenExpired)
        ));
        let expiry = LoginExpiry::new(
            Utc::now() + chrono::Duration::seconds(60),
            Duration::from_secs(5),
        )
        .unwrap();
        tokio::time::advance(Duration::from_secs(56)).await;
        assert!(matches!(
            expiry.verify(Duration::from_secs(5)),
            Err(PoolError::TokenExpired)
        ));
    }
}

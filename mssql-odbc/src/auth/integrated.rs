// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `Authentication=ActiveDirectoryIntegrated` — an Entra token for the
//! operating-system identity, with no prompt and no secret in the connection
//! string.
//!
//! The mechanism differs by platform, as it does in msodbcsql:
//!
//! - **Windows** asks `mssql-auth.dll` (OneAuth) to sign in as the logged-on
//!   Windows account ([`super::msqa::acquire_integrated_token`]), the path
//!   `SNISecMSQAGetAccessToken` takes for `MSQAAuthMethod::ActiveDirectoryIntegrated`.
//! - **Linux and macOS** authenticate to the tenant's ADFS with the Kerberos
//!   ticket from `kinit` and exchange the resulting SAML assertion for a token
//!   ([`super::federated`]), the `AzureADAuth` path msodbcsql compiles there.
//!
//! No token is cached: every connection acquires afresh, as msodbcsql does.

use async_trait::async_trait;
use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use mssql_tds::core::TdsResult;

#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use mssql_tds::connection::client_context::DEFAULT_CONNECT_TIMEOUT_SECS;
#[cfg(windows)]
use mssql_tds::error::Error;
#[cfg(windows)]
use mssql_tds::security::SecurityError;
#[cfg(windows)]
use tracing::debug;

/// msodbcsql's Entra application id and redirect URI for the MSQA path
/// (`Parse.cpp:3606-3608`).
#[cfg(windows)]
const PUBLIC_CLIENT_ID: &str = "2c1229aa-16c5-4ff5-b46b-4f7fe2a2a9c8";
#[cfg(windows)]
const REDIRECT_URI: &str = "https://sqlaad/";

/// Acquires access tokens for the operating-system identity.
#[derive(Clone)]
pub(crate) struct IntegratedTokenFactory {
    /// How long transient OneAuth failures are retried. msodbcsql retries
    /// while the login timeout has not elapsed (`Parse.cpp:3641-3647`).
    #[cfg(windows)]
    retry_budget: Duration,
}

impl IntegratedTokenFactory {
    /// `login_timeout_secs` bounds transient retries on Windows. An unset or
    /// zero (unlimited) login timeout falls back to the default connect
    /// timeout so a persistently "transient" failure cannot retry forever.
    #[cfg_attr(not(windows), expect(unused_variables))]
    pub(crate) fn new(login_timeout_secs: Option<u32>) -> Self {
        Self {
            #[cfg(windows)]
            retry_budget: Duration::from_secs(u64::from(
                login_timeout_secs
                    .filter(|&secs| secs > 0)
                    .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
            )),
        }
    }

    #[cfg(windows)]
    async fn acquire(&self, spn: String, sts_url: String) -> TdsResult<String> {
        let started = Instant::now();
        let mut wait = Duration::from_millis(100);
        loop {
            let (sts, resource) = (sts_url.clone(), spn.clone());
            let attempt = tokio::task::spawn_blocking(move || {
                super::msqa::acquire_integrated_token(
                    &sts,
                    &resource,
                    PUBLIC_CLIENT_ID,
                    REDIRECT_URI,
                )
            })
            .await
            .map_err(|e| {
                Error::Security(SecurityError::InternalError(format!(
                    "Entra integrated authentication did not run to completion: {e}"
                )))
            })?;
            match attempt {
                // `msqa` reports only OneAuth's transient statuses as
                // `ConnectionError`.
                Err(Error::ConnectionError(message)) if started.elapsed() < self.retry_budget => {
                    debug!(%message, "integrated: transient failure, retrying");
                    tokio::time::sleep(wait).await;
                    wait += wait;
                }
                other => return other,
            }
        }
    }

    #[cfg(not(windows))]
    async fn acquire(&self, spn: String, sts_url: String) -> TdsResult<String> {
        let transport = unix::ReqwestTransport::new()?;
        super::federated::acquire_token(
            &transport,
            std::sync::Arc::new(unix::GssapiIdentity),
            &sts_url,
            &spn,
            super::federated::RetryPolicy::default(),
        )
        .await
    }
}

#[async_trait]
impl EntraIdTokenFactory for IntegratedTokenFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        let token = self.acquire(spn, sts_url).await?;
        Ok(super::entra::encode_utf16le(&token))
    }
}

/// Production transport and identity for the federated flow.
#[cfg(not(windows))]
mod unix {
    use async_trait::async_trait;
    use mssql_tds::core::TdsResult;
    use mssql_tds::error::Error;
    use mssql_tds::security::unix::{GssapiContext, default_principal_name};
    use mssql_tds::security::{SecurityContext, SecurityError};

    use super::super::federated::{
        HttpMethod, HttpRequest, HttpResponse, HttpTransport, KerberosIdentity,
    };

    /// reqwest configured like msodbcsql's curl handle: https only, no
    /// redirects, and the same identifying headers.
    pub(super) struct ReqwestTransport {
        client: reqwest::Client,
    }

    impl ReqwestTransport {
        pub(super) fn new() -> TdsResult<Self> {
            let client = reqwest::Client::builder()
                .https_only(true)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("AzureADAuthProvider")
                .build()
                .map_err(|e| {
                    Error::Security(SecurityError::InternalError(format!(
                        "could not create the HTTP client for Entra integrated authentication: {e}"
                    )))
                })?;
            Ok(Self { client })
        }
    }

    #[async_trait]
    impl HttpTransport for ReqwestTransport {
        async fn send(&self, request: &HttpRequest) -> TdsResult<HttpResponse> {
            let mut builder = match request.method {
                HttpMethod::Get => self.client.get(&request.url),
                HttpMethod::Post => self.client.post(&request.url),
            }
            .header("Pragma", "no-cache")
            .header("Accept", "*/*");
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = &request.body {
                builder = builder.body(body.clone());
            }
            let transport_error = |e: reqwest::Error| {
                Error::ConnectionError(format!(
                    "Entra integrated authentication could not reach {}: {e}",
                    request.url
                ))
            };
            let response = builder.send().await.map_err(transport_error)?;
            let status = response.status().as_u16();
            let www_authenticate = response
                .headers()
                .get_all(reqwest::header::WWW_AUTHENTICATE)
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_string))
                .collect();
            let body = response.text().await.map_err(transport_error)?;
            Ok(HttpResponse {
                status,
                www_authenticate,
                body,
            })
        }
    }

    /// The default credential cache, through mssql-tds's GSSAPI bindings.
    pub(super) struct GssapiIdentity;

    impl KerberosIdentity for GssapiIdentity {
        fn principal(&self) -> TdsResult<String> {
            default_principal_name().map_err(Error::Security)
        }

        fn negotiate_context(&self, host: &str) -> TdsResult<Box<dyn SecurityContext>> {
            let context = GssapiContext::for_http_negotiate(host).map_err(Error::Security)?;
            Ok(Box::new(context))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use mssql_mock_tds::MockTdsServer;
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

    use super::super::entra::configure_auth;
    use crate::connection::odbc_authentication_transformer::transform_auth;

    #[cfg(windows)]
    #[test]
    fn client_id_matches_msodbcsql() {
        // Parse.cpp:3606-3608.
        assert_eq!(PUBLIC_CLIENT_ID, "2c1229aa-16c5-4ff5-b46b-4f7fe2a2a9c8");
        assert_eq!(REDIRECT_URI, "https://sqlaad/");
    }

    #[cfg(windows)]
    #[test]
    fn retry_budget_follows_the_login_timeout() {
        assert_eq!(
            IntegratedTokenFactory::new(Some(30)).retry_budget,
            Duration::from_secs(30)
        );
        let default = Duration::from_secs(u64::from(DEFAULT_CONNECT_TIMEOUT_SECS));
        assert_eq!(IntegratedTokenFactory::new(None).retry_budget, default);
        assert_eq!(IntegratedTokenFactory::new(Some(0)).retry_budget, default);
    }

    type Calls = Arc<Mutex<Vec<(String, String, TdsAuthenticationMethod)>>>;

    /// Stands in for the OS-identity acquisition, which needs a real tenant.
    #[derive(Clone)]
    struct RecordingFactory {
        calls: Calls,
    }

    #[async_trait]
    impl EntraIdTokenFactory for RecordingFactory {
        async fn create_token(
            &self,
            spn: String,
            sts_url: String,
            auth_method: TdsAuthenticationMethod,
        ) -> TdsResult<Vec<u8>> {
            self.calls
                .lock()
                .expect("call capture mutex poisoned")
                .push((spn, sts_url, auth_method));
            Ok(super::super::entra::encode_utf16le(MOCK_TOKEN))
        }
    }

    const MOCK_TOKEN: &str = "integrated.mock.token";
    const MSAL_WORKFLOW_INTEGRATED: u8 = 0x02;

    /// Drives `keyword` from the connection string through the driver's
    /// transform and configure steps and a real LOGIN7 against the mock
    /// server, with only the token acquisition replaced.
    fn connect_with(keyword: &str) {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let server = MockTdsServer::new("127.0.0.1:0")
                .await
                .expect("mock server");
            let addr = server.local_addr();
            let store = server.connection_store();
            let (shutdown, rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let _ = server.run_with_shutdown(rx).await;
            });

            // UID/PWD must not reach LOGIN7: the OS identity is the credential.
            let resolved = transform_auth(Some(keyword), None, "user@contoso.com", "pw", None);
            let mut context = ClientContext::default();
            configure_auth(&mut context, resolved, "mock").expect("supported");
            assert!(context.user_name.is_empty() && context.password.is_empty());

            let calls = Calls::default();
            let slot = context
                .auth_method_map
                .get_mut(&TdsAuthenticationMethod::ActiveDirectoryIntegrated)
                .expect("the factory must be keyed where mssql-tds looks it up");
            *slot = Box::new(RecordingFactory {
                calls: Arc::clone(&calls),
            });
            context.database = "master".to_string();
            context.encryption_options = EncryptionOptions {
                mode: EncryptionSetting::PreferOff,
                trust_server_certificate: true,
                host_name_in_cert: None,
                server_certificate: None,
            };

            let datasource = format!("tcp:{},{}", addr.ip(), addr.port());
            let client = TdsConnectionProvider {}
                .create_client(context, &datasource, None)
                .await
                .expect("login");

            {
                let calls = calls.lock().expect("call capture mutex poisoned");
                assert_eq!(calls.len(), 1, "one token per connection, no caching");
                assert_eq!(
                    calls[0].2,
                    TdsAuthenticationMethod::ActiveDirectoryIntegrated
                );
                assert!(calls[0].1.starts_with("https://"), "STS from FEDAUTHINFO");
            }
            {
                let store = store.lock().await;
                let info = store.all().values().last().expect("recorded connection");
                assert_eq!(info.fedauth_workflow, Some(MSAL_WORKFLOW_INTEGRATED));
                assert_eq!(info.received_token_as_string().as_deref(), Some(MOCK_TOKEN));
            }

            drop(client);
            let _ = shutdown.send(());
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        });
    }

    #[test]
    fn integrated_logs_in_with_the_integrated_workflow() {
        connect_with("ActiveDirectoryIntegrated");
    }

    #[cfg(not(windows))]
    #[test]
    fn interactive_off_windows_logs_in_as_integrated() {
        connect_with("ActiveDirectoryInteractive");
    }
}

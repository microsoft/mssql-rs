// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Entra ID token acquisition for `-G` and `--authentication-method`.
//!
//! `mssql-tds` asks a registered [`EntraIdTokenFactory`] for a bearer token
//! during the federated-authentication handshake. Without one, a connection
//! that negotiates FedAuth fails at login with no token to send, so every
//! method this tool accepts must have a factory registered for it here.
//!
//! Most methods map onto an `azure_identity` credential. Three do not, because
//! the Rust SDK has no equivalent, and are implemented directly against the
//! OAuth2 token endpoint:
//!
//! - **Password** — resource-owner password credentials.
//! - **Device code** — the polling flow, which is what a headless session needs.
//!
//! Interactive browser sign-in is refused rather than approximated: it needs a
//! loopback redirect listener and a browser, neither of which belongs in a tool
//! that is usually run non-interactively.
//!
//! Security notes:
//! - The STS authority comes from the server's FEDAUTHINFO, and is where any
//!   secret is sent. Matching msodbcsql and the Azure SDK, the server-provided
//!   authority is trusted but must be `https`. On a channel that is not
//!   certificate-validated (`-C`), a hostile server could redirect a secret to
//!   an authority it controls — use `-N strict` or a validated certificate when
//!   authenticating with a secret.
//! - Secrets travel in the token-request body. Never log the request.

use std::sync::Arc;

use async_trait::async_trait;
use azure_core::cloud::{CloudConfiguration, CustomConfiguration};
use azure_core::credentials::{Secret, TokenCredential};
use azure_core::http::ClientOptions;
use azure_identity::{
    AzureCliCredential, AzureCliCredentialOptions, AzureDeveloperCliCredential,
    AzureDeveloperCliCredentialOptions, AzurePipelinesCredential, AzurePipelinesCredentialOptions,
    ClientAssertionCredential, ClientAssertionCredentialOptions, ClientSecretCredential,
    ClientSecretCredentialOptions, DeveloperToolsCredential, ManagedIdentityCredential,
    ManagedIdentityCredentialOptions, UserAssignedId, WorkloadIdentityCredential,
    WorkloadIdentityCredentialOptions,
};
use mssql_tds::connection::client_context::{
    ClientContext, EntraIdTokenFactory, TdsAuthenticationMethod,
};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;
use tokio::sync::OnceCell;
use url::{Position, Url};

mod oauth;

/// What the factory needs in order to acquire a token, captured from the
/// command line before connecting.
///
/// Deliberately not `Debug`: several variants hold a secret.
#[derive(Clone)]
pub struct Credentials {
    pub method: TdsAuthenticationMethod,
    /// `-U`. Depending on the method this is a user name, a client id, or
    /// `client-id@tenant-id`.
    pub user: String,
    /// `-P`. A password, a client secret, or a signed client assertion.
    pub secret: String,
}

/// Splits the `client-id@tenant-id` form the reference accepts in `-U`.
///
/// Without a tenant the caller gets whichever tenant the STS URL names, which
/// is the common case for a single-tenant server.
fn split_client_and_tenant(user: &str) -> (String, Option<String>) {
    match user.split_once('@') {
        // A trailing `@` names no tenant, but the client id still ends there.
        Some((client, tenant)) => (
            client.to_string(),
            (!tenant.is_empty()).then(|| tenant.to_string()),
        ),
        None => (user.to_string(), None),
    }
}

/// Acquires Entra ID access tokens during the FedAuth handshake.
#[derive(Clone)]
struct SqlcmdTokenFactory {
    credentials: Credentials,
    /// Built on the first token request and reused for the rest of the
    /// connection, so the credential's own token cache survives repeated
    /// logins (session recovery, for instance).
    credential: Arc<OnceCell<Arc<dyn TokenCredential>>>,
}

#[async_trait]
impl EntraIdTokenFactory for SqlcmdTokenFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        let scope = normalize_scope(&spn);

        // The two flows the Rust SDK does not cover talk to the token endpoint
        // directly, and are not `TokenCredential`s.
        match self.credentials.method {
            TdsAuthenticationMethod::ActiveDirectoryPassword => {
                let token = oauth::password_token(
                    &sts_url,
                    &self.credentials.user,
                    &self.credentials.secret,
                    &scope,
                )
                .await?;
                return Ok(encode_utf16le(&token));
            }
            TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow => {
                let token = oauth::device_code_token(&sts_url, &self.credentials.user, &scope)
                    .await
                    .map_err(|e| Error::ConnectionError(format!("device code flow failed: {e}")))?;
                return Ok(encode_utf16le(&token));
            }
            _ => {}
        }

        let credential = self
            .credential
            .get_or_try_init(|| async { self.build_credential(&sts_url) })
            .await?;

        let access_token = credential
            .get_token(&[scope.as_str()], None)
            .await
            .map_err(|e| {
                Error::ConnectionError(format!("Entra ID token acquisition failed: {e}"))
            })?;

        Ok(encode_utf16le(access_token.token.secret()))
    }
}

impl SqlcmdTokenFactory {
    fn build_credential(&self, sts_url: &str) -> TdsResult<Arc<dyn TokenCredential>> {
        use TdsAuthenticationMethod as M;

        let (authority_host, sts_tenant) = split_sts_url(sts_url)?;
        let (client_id, user_tenant) = split_client_and_tenant(&self.credentials.user);
        // A tenant named in `-U` wins over the one the server advertised.
        let tenant = user_tenant.unwrap_or(sts_tenant);

        // `CustomConfiguration` is `#[non_exhaustive]`, so it has to be built
        // by mutating a default rather than with a struct literal.
        let mut custom = CustomConfiguration::default();
        custom.authority_host = authority_host;
        let client_options = ClientOptions {
            cloud: Some(Arc::new(CloudConfiguration::Custom(custom))),
            ..Default::default()
        };

        let failed = |what: &str, e: azure_core::Error| {
            Error::ConnectionError(format!("failed to build {what} credential: {e}"))
        };

        let credential: Arc<dyn TokenCredential> = match self.credentials.method {
            M::ActiveDirectoryServicePrincipal => ClientSecretCredential::new(
                &tenant,
                client_id,
                Secret::from(self.credentials.secret.clone()),
                Some(ClientSecretCredentialOptions { client_options }),
            )
            .map_err(|e| failed("service-principal", e))?,

            M::ActiveDirectoryManagedIdentity | M::ActiveDirectoryMSI => {
                // A non-empty `-U` selects a user-assigned identity by client id.
                let user_assigned_id =
                    (!client_id.is_empty()).then(|| UserAssignedId::ClientId(client_id.clone()));
                ManagedIdentityCredential::new(Some(ManagedIdentityCredentialOptions {
                    user_assigned_id,
                    ..Default::default()
                }))
                .map_err(|e| failed("managed-identity", e))?
            }

            M::ActiveDirectoryAzCli => AzureCliCredential::new(Some(AzureCliCredentialOptions {
                tenant_id: Some(tenant),
                ..Default::default()
            }))
            .map_err(|e| failed("Azure CLI", e))?,

            M::ActiveDirectoryAzureDeveloperCli => {
                AzureDeveloperCliCredential::new(Some(AzureDeveloperCliCredentialOptions {
                    tenant_id: Some(tenant),
                    ..Default::default()
                }))
                .map_err(|e| failed("Azure Developer CLI", e))?
            }

            M::ActiveDirectoryWorkloadIdentity => {
                WorkloadIdentityCredential::new(Some(WorkloadIdentityCredentialOptions {
                    client_id: (!client_id.is_empty()).then_some(client_id),
                    tenant_id: Some(tenant),
                    credential_options: ClientAssertionCredentialOptions { client_options },
                    ..Default::default()
                }))
                .map_err(|e| failed("workload-identity", e))?
            }

            M::ActiveDirectoryClientAssertion => {
                // `-P` carries the signed assertion itself.
                let assertion = self.credentials.secret.clone();
                ClientAssertionCredential::new(
                    tenant,
                    client_id,
                    oauth::StaticAssertion(assertion),
                    Some(ClientAssertionCredentialOptions { client_options }),
                )
                .map_err(|e| failed("client-assertion", e))?
            }

            M::ActiveDirectoryAzurePipelines => {
                // The pipeline supplies the service connection and its token
                // through the environment; there is nowhere on the sqlcmd
                // command line to put them.
                let service_connection_id =
                    std::env::var("AZURESUBSCRIPTION_SERVICE_CONNECTION_ID").map_err(|_| {
                        Error::ConnectionError(
                            "ActiveDirectoryAzurePipelines requires \
                             AZURESUBSCRIPTION_SERVICE_CONNECTION_ID to be set."
                                .to_string(),
                        )
                    })?;
                let system_access_token = std::env::var("SYSTEM_ACCESSTOKEN").map_err(|_| {
                    Error::ConnectionError(
                        "ActiveDirectoryAzurePipelines requires SYSTEM_ACCESSTOKEN to be set."
                            .to_string(),
                    )
                })?;
                AzurePipelinesCredential::new(
                    tenant,
                    client_id,
                    &service_connection_id,
                    Secret::from(system_access_token),
                    Some(AzurePipelinesCredentialOptions {
                        credential_options: ClientAssertionCredentialOptions { client_options },
                    }),
                )
                .map_err(|e| failed("Azure Pipelines", e))?
            }

            M::ActiveDirectoryEnvironment => {
                // The Rust SDK has no EnvironmentCredential, but the contract is
                // just "read the standard AZURE_* variables", so it is built
                // here from the same three the other SDKs read.
                let client_id = std::env::var("AZURE_CLIENT_ID").map_err(|_| {
                    Error::ConnectionError(
                        "ActiveDirectoryEnvironment requires AZURE_CLIENT_ID to be set."
                            .to_string(),
                    )
                })?;
                let tenant_id = std::env::var("AZURE_TENANT_ID").unwrap_or(tenant);
                let secret = std::env::var("AZURE_CLIENT_SECRET").map_err(|_| {
                    Error::ConnectionError(
                        "ActiveDirectoryEnvironment requires AZURE_CLIENT_SECRET to be set."
                            .to_string(),
                    )
                })?;
                ClientSecretCredential::new(
                    &tenant_id,
                    client_id,
                    Secret::from(secret),
                    Some(ClientSecretCredentialOptions { client_options }),
                )
                .map_err(|e| failed("environment", e))?
            }

            // `ActiveDirectoryDefault` and anything else that reaches here walks
            // the developer-tools chain, which is the closest the Rust SDK has
            // to `DefaultAzureCredential`.
            _ => DeveloperToolsCredential::new(None).map_err(|e| failed("default", e))?,
        };

        Ok(credential)
    }
}

/// Registers a token factory on `context` for the method it is configured with.
///
/// Called before connecting. No token is acquired here — that happens during
/// the handshake, once the server has named the authority to ask.
pub fn register(context: &mut ClientContext, credentials: Credentials) {
    let method = credentials.method.clone();
    // Methods that carry their own credential in LOGIN7 rather than a bearer
    // token need no factory.
    if matches!(
        method,
        TdsAuthenticationMethod::Password
            | TdsAuthenticationMethod::SSPI
            | TdsAuthenticationMethod::AccessToken
            | TdsAuthenticationMethod::ActiveDirectoryIntegrated
    ) {
        return;
    }
    let factory = SqlcmdTokenFactory {
        credentials,
        credential: Arc::new(OnceCell::new()),
    };
    context.auth_method_map.insert(method, Box::new(factory));
}

/// Normalizes an SPN into a v2 scope by ensuring exactly one `/.default`
/// suffix, e.g. `https://database.windows.net/` becomes
/// `https://database.windows.net/.default`.
fn normalize_scope(spn: &str) -> String {
    let trimmed = spn.trim_end_matches('/');
    if trimmed.ends_with("/.default") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/.default")
    }
}

/// Splits an STS URL such as `https://login.microsoftonline.com/<tenant>` into
/// its authority and tenant.
///
/// Requires `https`: a secret is sent to this authority, so an unencrypted
/// endpoint is refused.
fn split_sts_url(sts_url: &str) -> TdsResult<(String, String)> {
    let url = Url::parse(sts_url.trim())
        .map_err(|e| Error::ConnectionError(format!("invalid STS URL: {sts_url} ({e})")))?;
    if url.scheme() != "https" {
        return Err(Error::ConnectionError(format!(
            "STS URL must use https: {sts_url}"
        )));
    }
    let authority = url[..Position::BeforePath].to_string();
    let tenant = url
        .path_segments()
        .and_then(|mut segments| segments.next())
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| Error::ConnectionError(format!("STS URL is missing a tenant: {sts_url}")))?
        .to_string();
    Ok((authority, tenant))
}

/// Encodes a string as UTF-16LE — the form the FedAuth token message carries.
fn encode_utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_gains_exactly_one_default_suffix() {
        assert_eq!(
            normalize_scope("https://database.windows.net/"),
            "https://database.windows.net/.default"
        );
        assert_eq!(
            normalize_scope("https://database.windows.net/.default"),
            "https://database.windows.net/.default"
        );
    }

    #[test]
    fn an_sts_url_splits_into_authority_and_tenant() {
        let (authority, tenant) =
            split_sts_url("https://login.microsoftonline.com/contoso.onmicrosoft.com").unwrap();
        assert_eq!(authority, "https://login.microsoftonline.com");
        assert_eq!(tenant, "contoso.onmicrosoft.com");
    }

    #[test]
    fn an_sts_url_without_https_is_refused() {
        // The client secret is sent here, so plaintext is not acceptable even
        // though the URL came from the server.
        assert!(split_sts_url("http://login.microsoftonline.com/tenant").is_err());
    }

    #[test]
    fn a_user_may_carry_its_tenant() {
        assert_eq!(
            split_client_and_tenant("client-id@tenant-id"),
            ("client-id".to_string(), Some("tenant-id".to_string()))
        );
        assert_eq!(
            split_client_and_tenant("just-a-client-id"),
            ("just-a-client-id".to_string(), None)
        );
        // A trailing `@` names no tenant, so the STS URL's tenant still applies.
        assert_eq!(
            split_client_and_tenant("client-id@"),
            ("client-id".to_string(), None)
        );
    }

    #[test]
    fn a_token_is_encoded_as_utf16le() {
        assert_eq!(encode_utf16le("ab"), vec![b'a', 0, b'b', 0]);
    }

    #[test]
    fn methods_that_carry_their_own_credential_register_no_factory() {
        for method in [
            TdsAuthenticationMethod::Password,
            TdsAuthenticationMethod::SSPI,
            TdsAuthenticationMethod::AccessToken,
            TdsAuthenticationMethod::ActiveDirectoryIntegrated,
        ] {
            let mut context = ClientContext::default();
            register(
                &mut context,
                Credentials {
                    method,
                    user: String::new(),
                    secret: String::new(),
                },
            );
            assert!(context.auth_method_map.is_empty());
        }
    }

    #[test]
    fn every_federated_method_registers_a_factory() {
        // A method that reaches the handshake without a factory fails at login
        // with nothing to send, so this guards the whole federated set.
        for method in [
            TdsAuthenticationMethod::ActiveDirectoryDefault,
            TdsAuthenticationMethod::ActiveDirectoryPassword,
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
            TdsAuthenticationMethod::ActiveDirectoryManagedIdentity,
            TdsAuthenticationMethod::ActiveDirectoryMSI,
            TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow,
            TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity,
            TdsAuthenticationMethod::ActiveDirectoryAzCli,
            TdsAuthenticationMethod::ActiveDirectoryAzureDeveloperCli,
            TdsAuthenticationMethod::ActiveDirectoryAzurePipelines,
            TdsAuthenticationMethod::ActiveDirectoryEnvironment,
            TdsAuthenticationMethod::ActiveDirectoryClientAssertion,
        ] {
            let mut context = ClientContext::default();
            register(
                &mut context,
                Credentials {
                    method: method.clone(),
                    user: "client-id".to_string(),
                    secret: "secret".to_string(),
                },
            );
            assert!(
                context.auth_method_map.contains_key(&method),
                "{method:?} registered no token factory"
            );
        }
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Entra ID token acquisition for the FedAuth handshake (mssql-odbc T2).
//!
//! [`EntraTokenFactory`] implements the mssql-tds [`EntraIdTokenFactory`] trait
//! using the Azure SDK for Rust (`azure_identity`). It is built from the
//! connection-string inputs in `do_connect`, registered in
//! `ClientContext::auth_method_map`, and invoked by mssql-tds during login when
//! the server requests a federated-authentication token.
//!
//! Security notes:
//! - The STS authority comes from the server's FEDAUTHINFO and is where the
//!   service-principal secret is sent. Matching msodbcsql (via
//!   `azure-identity-cpp`) and the Azure SDK, the driver trusts the
//!   server-provided authority but requires it to be `https`; the host is not
//!   otherwise restricted. Residual risk: on a channel that is not
//!   certificate-validated (`TrustServerCertificate=yes`), a rogue or
//!   man-in-the-middle server could redirect the secret to an attacker-owned
//!   authority — use `Encrypt=Strict` or a validated server certificate for
//!   service-principal auth.
//! - The service-principal secret travels in the Azure SDK token-request body;
//!   do not enable `azure_*` trace-level logging in production.

use std::sync::Arc;

use async_trait::async_trait;
use azure_core::cloud::{CloudConfiguration, CustomConfiguration};
use azure_core::credentials::{Secret, TokenCredential};
use azure_core::http::ClientOptions;
use azure_identity::{
    ClientSecretCredential, ClientSecretCredentialOptions, ManagedIdentityCredential,
    ManagedIdentityCredentialOptions, UserAssignedId,
};
use tokio::sync::OnceCell;
use url::{Position, Url};

use super::interactive::{CONNECT_TIMEOUT_SECS, InteractiveTokenFactory};
use crate::connection::odbc_authentication_transformer::TransformedAuth;
use mssql_tds::connection::client_context::{
    ClientContext, EntraIdTokenFactory, TdsAuthenticationMethod,
};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;

/// Credentials captured from the connection string, used to acquire a token.
///
/// Deliberately not `Debug`: the service-principal secret must never be logged.
#[derive(Clone)]
pub(crate) enum CredentialConfig {
    /// Service principal with a client secret (`UID` = client id, `PWD` = secret).
    ServicePrincipalSecret { client_id: String, secret: Secret },
    /// Managed identity. `client_id` selects a user-assigned identity by its
    /// client id (the ODBC `UID` convention); object/resource ids are not
    /// supported. `None` uses the system-assigned identity.
    ManagedIdentity { client_id: Option<String> },
}

/// Acquires Entra ID access tokens via the Azure SDK during the FedAuth handshake.
#[derive(Clone)]
pub(crate) struct EntraTokenFactory {
    config: CredentialConfig,
    /// The Azure SDK credential, built lazily on the first token request and
    /// reused for the remaining requests on this connection so the credential's
    /// in-memory token cache is shared across repeated logins (e.g. session
    /// recovery). Cross-connection reuse is tracked in AB#46409.
    credential: Arc<OnceCell<Arc<dyn TokenCredential>>>,
}

impl EntraTokenFactory {
    pub(crate) fn new(config: CredentialConfig) -> Self {
        Self {
            config,
            credential: Arc::new(OnceCell::new()),
        }
    }

    /// Builds the Azure SDK credential for the configured method. For a service
    /// principal the server-provided STS URL selects the authority host; managed
    /// identity resolves via IMDS and ignores it.
    fn build_credential(&self, sts_url: &str) -> TdsResult<Arc<dyn TokenCredential>> {
        match &self.config {
            CredentialConfig::ServicePrincipalSecret { client_id, secret } => {
                let (authority_host, tenant_id) = split_sts_url(sts_url)?;
                // `CustomConfiguration` is `#[non_exhaustive]`, so it must be
                // built by mutating a default (a struct literal cannot name a
                // non_exhaustive foreign type); the field-reassign lint does not
                // fire on it.
                let mut custom = CustomConfiguration::default();
                custom.authority_host = authority_host;
                let client_options = ClientOptions {
                    cloud: Some(Arc::new(CloudConfiguration::Custom(custom))),
                    ..Default::default()
                };
                let credential: Arc<dyn TokenCredential> = ClientSecretCredential::new(
                    &tenant_id,
                    client_id.clone(),
                    secret.clone(),
                    Some(ClientSecretCredentialOptions { client_options }),
                )
                .map_err(|e| {
                    Error::ConnectionError(format!(
                        "failed to build service-principal credential: {e}"
                    ))
                })?;
                Ok(credential)
            }
            CredentialConfig::ManagedIdentity { client_id } => {
                // A non-empty client id selects a user-assigned identity.
                let user_assigned_id = client_id
                    .as_ref()
                    .filter(|id| !id.is_empty())
                    .map(|id| UserAssignedId::ClientId(id.clone()));
                let credential: Arc<dyn TokenCredential> =
                    ManagedIdentityCredential::new(Some(ManagedIdentityCredentialOptions {
                        user_assigned_id,
                        ..Default::default()
                    }))
                    .map_err(|e| {
                        Error::ConnectionError(format!(
                            "failed to build managed-identity credential: {e}"
                        ))
                    })?;
                Ok(credential)
            }
        }
    }
}

#[async_trait]
impl EntraIdTokenFactory for EntraTokenFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        let scope = normalize_scope(&spn);
        let scopes: &[&str] = &[scope.as_str()];

        // Build the credential once per connection and reuse it, so the Azure
        // SDK credential's own token cache is shared across this connection's
        // token requests instead of re-authenticating every login.
        let credential = self
            .credential
            .get_or_try_init(|| async { self.build_credential(&sts_url) })
            .await?;

        let access_token = credential.get_token(scopes, None).await.map_err(|e| {
            Error::ConnectionError(format!("Entra ID token acquisition failed: {e}"))
        })?;

        Ok(encode_utf16le(access_token.token.secret()))
    }
}

/// Normalizes an SPN/resource into a v2 scope by ensuring a single `/.default`
/// suffix (e.g. `https://database.windows.net/` becomes
/// `https://database.windows.net/.default`).
pub(super) fn normalize_scope(spn: &str) -> String {
    let trimmed = spn.trim_end_matches('/');
    if trimmed.ends_with("/.default") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/.default")
    }
}

/// Splits an STS URL such as `https://login.microsoftonline.com/<tenant>` into
/// its authority (`https://login.microsoftonline.com`) and tenant.
///
/// Requires `https` — the client secret is sent to this authority, so an
/// unencrypted endpoint is rejected. The host is not otherwise restricted:
/// matching msodbcsql (via `azure-identity-cpp`) and the Azure SDK, the
/// server-provided authority is trusted. See the module-level security note on
/// the residual risk when the TDS channel is not certificate-validated.
///
/// Parsing goes through the `url` crate (WHATWG): the scheme and host are
/// lowercased and the default `:443` port is dropped.
pub(super) fn split_sts_url(sts_url: &str) -> TdsResult<(String, String)> {
    // The URL is server-provided (FEDAUTHINFO): tolerate surrounding whitespace.
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

/// Encodes a string as UTF-16LE bytes — the token format the FedAuth token
/// message carries on the wire.
pub(super) fn encode_utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// Applies the resolved authentication to `context`: sets credentials for
/// SQL/SSPI, the pre-acquired token for `AccessToken`, or builds and registers
/// an Entra token factory for service principal / managed identity. For the
/// factory methods the credentials are captured by the factory and left out of
/// `context`, so they are never serialized in LOGIN7.
///
/// Network-free: no token is acquired here. Returns the unsupported method as
/// `Err` so the caller can surface `HYC00`.
pub(crate) fn configure_auth(
    context: &mut ClientContext,
    resolved: TransformedAuth,
) -> Result<(), TdsAuthenticationMethod> {
    // Resolve the method first and only commit it to the context on a supported
    // path, so the context is left untouched when we return `Err`.
    let method = resolved.method.clone();
    match resolved.method {
        TdsAuthenticationMethod::Password | TdsAuthenticationMethod::SSPI => {
            context.user_name = resolved.user_name;
            context.password = resolved.password;
        }
        TdsAuthenticationMethod::AccessToken => {
            context.access_token = resolved.access_token;
        }
        TdsAuthenticationMethod::ActiveDirectoryServicePrincipal => {
            let factory = EntraTokenFactory::new(CredentialConfig::ServicePrincipalSecret {
                client_id: resolved.user_name,
                secret: Secret::from(resolved.password),
            });
            context.auth_method_map.insert(
                TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
                Box::new(factory),
            );
        }
        TdsAuthenticationMethod::ActiveDirectoryManagedIdentity => {
            // A non-empty UID selects a user-assigned identity (its client id).
            let client_id = (!resolved.user_name.is_empty()).then_some(resolved.user_name);
            let factory = EntraTokenFactory::new(CredentialConfig::ManagedIdentity { client_id });
            context.auth_method_map.insert(
                TdsAuthenticationMethod::ActiveDirectoryManagedIdentity,
                Box::new(factory),
            );
        }
        TdsAuthenticationMethod::ActiveDirectoryInteractive => {
            // A non-empty UID becomes the `login_hint`; the browser flow uses the
            // well-known public-client id and stores no secret in the context.
            let login_hint = (!resolved.user_name.is_empty()).then_some(resolved.user_name);
            let factory = InteractiveTokenFactory::new(login_hint);
            context.auth_method_map.insert(
                TdsAuthenticationMethod::ActiveDirectoryInteractive,
                Box::new(factory),
            );
            // The browser sign-in (with MFA) can take minutes, far longer than
            // the default 15s. `connect_timeout` bounds both the outer login
            // deadline and each TCP-connect attempt, so it must stay non-zero
            // (zero fails the TCP connect immediately); raise it to cover the
            // interactive flow. Mirrors SqlClient's enlarged Connect Timeout for
            // interactive auth. See AB#46067.
            context.connect_timeout = CONNECT_TIMEOUT_SECS;
        }
        other => return Err(other),
    }
    context.tds_authentication_method = method;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_appends_default_suffix() {
        assert_eq!(
            normalize_scope("https://database.windows.net/"),
            "https://database.windows.net/.default"
        );
        assert_eq!(
            normalize_scope("https://database.windows.net"),
            "https://database.windows.net/.default"
        );
    }

    #[test]
    fn scope_preserves_existing_default() {
        assert_eq!(
            normalize_scope("https://database.windows.net/.default"),
            "https://database.windows.net/.default"
        );
    }

    #[test]
    fn sts_url_microsoftonline_authority_and_tenant() {
        let (authority, tenant) =
            split_sts_url("https://login.microsoftonline.com/72f988bf-1234").unwrap();
        assert_eq!(authority, "https://login.microsoftonline.com");
        assert_eq!(tenant, "72f988bf-1234");
    }

    #[test]
    fn sts_url_windows_net_authority_and_tenant() {
        let (authority, tenant) = split_sts_url("https://login.windows.net/common/oauth2").unwrap();
        assert_eq!(authority, "https://login.windows.net");
        assert_eq!(tenant, "common");
    }

    #[test]
    fn sts_url_trailing_slash_ok() {
        let (authority, tenant) = split_sts_url("https://login.windows.net/my-tenant/").unwrap();
        assert_eq!(authority, "https://login.windows.net");
        assert_eq!(tenant, "my-tenant");
    }

    #[test]
    fn sts_url_missing_tenant_is_error() {
        assert!(split_sts_url("https://login.microsoftonline.com").is_err());
    }

    #[test]
    fn sts_url_missing_scheme_is_error() {
        assert!(split_sts_url("login.microsoftonline.com/tenant").is_err());
    }

    #[test]
    fn sts_url_rejects_non_https() {
        assert!(split_sts_url("http://login.microsoftonline.com/tenant").is_err());
    }

    #[test]
    fn sts_url_accepts_any_https_authority() {
        // Matches msodbcsql / the Azure SDK: the server-provided authority is
        // trusted as long as it is https (see the module security note). This
        // covers sovereign clouds and any other Entra-compatible endpoint.
        assert!(split_sts_url("https://login.microsoftonline.us/tenant").is_ok());
        assert!(split_sts_url("https://login.partner.microsoftonline.cn/tenant").is_ok());
        assert!(split_sts_url("https://sts.contoso.example/tenant").is_ok());
    }

    #[test]
    fn sts_url_default_https_port_normalized() {
        // WHATWG drops the default :443 port; harmless since 443 is the https
        // default. Non-default ports are preserved (see next test).
        let (authority, tenant) =
            split_sts_url("https://login.microsoftonline.com:443/my-tenant").unwrap();
        assert_eq!(authority, "https://login.microsoftonline.com");
        assert_eq!(tenant, "my-tenant");
    }

    #[test]
    fn sts_url_non_default_port_preserved() {
        let (authority, tenant) =
            split_sts_url("https://sts.contoso.example:8443/my-tenant").unwrap();
        assert_eq!(authority, "https://sts.contoso.example:8443");
        assert_eq!(tenant, "my-tenant");
    }

    #[test]
    fn sts_url_scheme_is_case_insensitive() {
        // URL schemes are case-insensitive; the reconstructed authority is
        // normalized to lowercase https.
        let (authority, tenant) =
            split_sts_url("HTTPS://login.microsoftonline.com/my-tenant").unwrap();
        assert_eq!(authority, "https://login.microsoftonline.com");
        assert_eq!(tenant, "my-tenant");
    }

    #[test]
    fn sts_url_trims_surrounding_whitespace() {
        let (authority, tenant) = split_sts_url("  https://login.windows.net/my-tenant  ").unwrap();
        assert_eq!(authority, "https://login.windows.net");
        assert_eq!(tenant, "my-tenant");
    }

    #[test]
    fn utf16le_encoding_is_little_endian() {
        // 'A' = U+0041 -> 0x41 0x00; 'AB' -> 0x41 0x00 0x42 0x00
        assert_eq!(encode_utf16le("A"), vec![0x41, 0x00]);
        assert_eq!(encode_utf16le("AB"), vec![0x41, 0x00, 0x42, 0x00]);
    }

    fn transformed(method: TdsAuthenticationMethod, uid: &str, pwd: &str) -> TransformedAuth {
        TransformedAuth {
            method,
            user_name: uid.to_string(),
            password: pwd.to_string(),
            access_token: None,
        }
    }

    #[test]
    fn configure_auth_service_principal_hides_credentials() {
        let mut ctx = ClientContext::default();
        let r = transformed(
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
            "client-id",
            "top-secret",
        );
        assert!(configure_auth(&mut ctx, r).is_ok());
        // Neither the client id nor the secret may be serialized in LOGIN7.
        assert!(ctx.user_name.is_empty());
        assert!(ctx.password.is_empty());
        assert!(
            ctx.auth_method_map
                .contains_key(&TdsAuthenticationMethod::ActiveDirectoryServicePrincipal)
        );
    }

    #[test]
    fn configure_auth_managed_identity_registers_factory() {
        let mut ctx = ClientContext::default();
        let r = transformed(
            TdsAuthenticationMethod::ActiveDirectoryManagedIdentity,
            "",
            "",
        );
        assert!(configure_auth(&mut ctx, r).is_ok());
        assert!(ctx.user_name.is_empty());
        assert!(
            ctx.auth_method_map
                .contains_key(&TdsAuthenticationMethod::ActiveDirectoryManagedIdentity)
        );
    }

    #[test]
    fn configure_auth_password_keeps_credentials() {
        let mut ctx = ClientContext::default();
        let r = transformed(TdsAuthenticationMethod::Password, "sa", "pw");
        assert!(configure_auth(&mut ctx, r).is_ok());
        assert_eq!(ctx.user_name, "sa");
        assert_eq!(ctx.password, "pw");
        assert!(ctx.auth_method_map.is_empty());
    }

    #[test]
    fn configure_auth_interactive_registers_factory() {
        let mut ctx = ClientContext::default();
        // UID is kept as the login hint; no secret is written to the context.
        let r = transformed(
            TdsAuthenticationMethod::ActiveDirectoryInteractive,
            "user@contoso.com",
            "",
        );
        assert!(configure_auth(&mut ctx, r).is_ok());
        assert!(ctx.user_name.is_empty());
        assert!(ctx.password.is_empty());
        assert!(
            ctx.auth_method_map
                .contains_key(&TdsAuthenticationMethod::ActiveDirectoryInteractive)
        );
        assert_eq!(
            ctx.tds_authentication_method,
            TdsAuthenticationMethod::ActiveDirectoryInteractive
        );
        // Interactive raises the login-connect budget (never zero, which would
        // fail the TCP connect) so the browser/MFA flow has time to complete.
        assert_eq!(ctx.connect_timeout, CONNECT_TIMEOUT_SECS);
        assert!(ctx.connect_timeout > 15);
    }

    #[test]
    fn configure_auth_unsupported_method_is_err() {
        let mut ctx = ClientContext::default();
        let r = transformed(
            TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow,
            "",
            "",
        );
        assert_eq!(
            configure_auth(&mut ctx, r),
            Err(TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow)
        );
    }
}

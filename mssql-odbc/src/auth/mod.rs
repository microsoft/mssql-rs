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

use async_trait::async_trait;
use azure_core::credentials::{Secret, TokenCredential};
use azure_identity::{
    ClientSecretCredential, ClientSecretCredentialOptions, ManagedIdentityCredential,
    ManagedIdentityCredentialOptions, TokenCredentialOptions, UserAssignedId,
};

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
}

impl EntraTokenFactory {
    pub(crate) fn new(config: CredentialConfig) -> Self {
        Self { config }
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

        let access_token = match &self.config {
            CredentialConfig::ServicePrincipalSecret { client_id, secret } => {
                let (authority_host, tenant_id) = split_sts_url(&sts_url)?;
                let mut credential_options = TokenCredentialOptions::default();
                credential_options.set_authority_host(authority_host);
                let credential = ClientSecretCredential::new(
                    &tenant_id,
                    client_id.clone(),
                    secret.clone(),
                    Some(ClientSecretCredentialOptions { credential_options }),
                )
                .map_err(|e| {
                    Error::ConnectionError(format!(
                        "failed to build service-principal credential: {e}"
                    ))
                })?;
                credential.get_token(scopes, None).await
            }
            CredentialConfig::ManagedIdentity { client_id } => {
                // Managed identity resolves tokens via IMDS; the STS authority
                // host is not used, so it is left at the default.
                let user_assigned_id = client_id
                    .as_ref()
                    .filter(|id| !id.is_empty())
                    .map(|id| UserAssignedId::ClientId(id.clone()));
                let credential =
                    ManagedIdentityCredential::new(Some(ManagedIdentityCredentialOptions {
                        credential_options: TokenCredentialOptions::default(),
                        user_assigned_id,
                    }))
                    .map_err(|e| {
                        Error::ConnectionError(format!(
                            "failed to build managed-identity credential: {e}"
                        ))
                    })?;
                credential.get_token(scopes, None).await
            }
        };

        let access_token = access_token.map_err(|e| {
            Error::ConnectionError(format!("Entra ID token acquisition failed: {e}"))
        })?;

        Ok(encode_utf16le(access_token.token.secret()))
    }
}

/// Normalizes an SPN/resource into a v2 scope by ensuring a single `/.default`
/// suffix (e.g. `https://database.windows.net/` becomes
/// `https://database.windows.net/.default`).
fn normalize_scope(spn: &str) -> String {
    let trimmed = spn.trim_end_matches('/');
    if trimmed.ends_with("/.default") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/.default")
    }
}

/// Splits an STS URL such as `https://login.microsoftonline.com/<tenant>` into
/// its authority host (`https://login.microsoftonline.com`) and tenant.
///
/// Requires `https` — the client secret is sent to this authority, so an
/// unencrypted endpoint is rejected. The host is not otherwise restricted:
/// matching msodbcsql (via `azure-identity-cpp`) and the Azure SDK, the
/// server-provided authority is trusted. See the module-level security note on
/// the residual risk when the TDS channel is not certificate-validated.
fn split_sts_url(sts_url: &str) -> TdsResult<(String, String)> {
    let trimmed = sts_url.trim_end_matches('/');
    let after_scheme = trimmed
        .strip_prefix("https://")
        .ok_or_else(|| Error::ConnectionError(format!("STS URL must use https: {sts_url}")))?;
    let (host, rest) = after_scheme
        .split_once('/')
        .ok_or_else(|| Error::ConnectionError(format!("STS URL is missing a tenant: {sts_url}")))?;
    let tenant = rest.split('/').next().unwrap_or_default();
    if host.is_empty() || tenant.is_empty() {
        return Err(Error::ConnectionError(format!(
            "STS URL is missing a host or tenant: {sts_url}"
        )));
    }
    Ok((format!("https://{host}"), tenant.to_string()))
}

/// Encodes a string as UTF-16LE bytes — the token format the FedAuth token
/// message carries on the wire.
fn encode_utf16le(s: &str) -> Vec<u8> {
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
    context.tds_authentication_method = resolved.method.clone();
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
        other => return Err(other),
    }
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
    fn sts_url_host_with_port_ok() {
        let (authority, tenant) =
            split_sts_url("https://login.microsoftonline.com:443/my-tenant").unwrap();
        assert_eq!(authority, "https://login.microsoftonline.com:443");
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
    fn configure_auth_unsupported_method_is_err() {
        let mut ctx = ClientContext::default();
        let r = transformed(TdsAuthenticationMethod::ActiveDirectoryInteractive, "", "");
        assert_eq!(
            configure_auth(&mut ctx, r),
            Err(TdsAuthenticationMethod::ActiveDirectoryInteractive)
        );
    }
}

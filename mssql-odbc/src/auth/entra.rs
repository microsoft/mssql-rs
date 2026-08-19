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
//! - Credentials (including the service-principal secret) are cached
//!   process-wide, keyed by identity (see `CREDENTIAL_CACHE`), so a secret can
//!   now outlive the connection that first supplied it for as long as the
//!   process runs — the standard trade-off for avoiding a per-connection AAD
//!   round-trip. Never `Debug`-format a `CredentialConfig` or log a cache key's
//!   inputs; the cache key itself carries only a digest of the secret.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use azure_core::cloud::{CloudConfiguration, CustomConfiguration};
use azure_core::credentials::{Secret, TokenCredential};
use azure_core::http::ClientOptions;
use azure_identity::{
    ClientSecretCredential, ClientSecretCredentialOptions, ManagedIdentityCredential,
    ManagedIdentityCredentialOptions, UserAssignedId,
};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};
use url::{Position, Url};

#[cfg(windows)]
use super::interactive::{InteractiveTokenFactory, LOGIN_TIMEOUT_SECS};
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

    /// Derives the process-wide cache key for this factory's identity. For a
    /// service principal this is the authority/tenant (from the server-provided
    /// STS URL), client id, and a SHA-256 digest of the secret — never the
    /// secret itself. The digest must be collision-resistant: unlike a
    /// `DefaultHasher`/SipHash digest, which is only DoS-resistant and would
    /// let two different secrets alias into the same cache entry, SHA-256
    /// makes that practically impossible, so a secret rotation always misses
    /// the old entry instead of silently continuing to authenticate with a
    /// stale (possibly revoked) secret. Managed identity keys on client id
    /// alone (empty for the system-assigned identity): IMDS is scoped to the
    /// local machine, so no authority/tenant applies.
    fn cache_key(&self, sts_url: &str) -> TdsResult<CredentialCacheKey> {
        match &self.config {
            CredentialConfig::ServicePrincipalSecret { client_id, secret } => {
                let (authority_host, tenant_id) = split_sts_url(sts_url)?;
                Ok(CredentialCacheKey::ServicePrincipal {
                    authority_host,
                    tenant_id,
                    client_id: client_id.clone(),
                    secret_digest: digest(secret.secret()),
                })
            }
            CredentialConfig::ManagedIdentity { client_id } => {
                Ok(CredentialCacheKey::ManagedIdentity {
                    client_id: client_id.clone().unwrap_or_default(),
                })
            }
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

        // Reuse the process-wide credential for this identity instead of
        // building a new one (and re-authenticating) on every connection.
        let key = self.cache_key(&sts_url)?;
        let credential = cached_credential(key, || self.build_credential(&sts_url))?;

        let access_token = credential.get_token(scopes, None).await.map_err(|e| {
            Error::ConnectionError(format!("Entra ID token acquisition failed: {e}"))
        })?;

        Ok(encode_utf16le(access_token.token.secret()))
    }
}

/// Identifies a distinct Entra ID identity for the process-wide credential
/// cache ([`CREDENTIAL_CACHE`]). Two factories that produce equal keys are
/// guaranteed to represent the same tenant/client/secret (or the same managed
/// identity) and may safely share one credential instance.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum CredentialCacheKey {
    ServicePrincipal {
        authority_host: String,
        tenant_id: String,
        client_id: String,
        secret_digest: [u8; 32],
    },
    ManagedIdentity {
        /// Empty for the system-assigned identity.
        client_id: String,
    },
}

impl CredentialCacheKey {
    /// A static, identifier-free label for tracing: enough to tell which
    /// auth method a cache hit/miss was for without logging any part of the
    /// identity itself (tenant, client id, or secret digest).
    fn label(&self) -> &'static str {
        match self {
            CredentialCacheKey::ServicePrincipal { .. } => "service principal",
            CredentialCacheKey::ManagedIdentity { .. } => "managed identity",
        }
    }
}

/// Collision-resistant digest used only as a process-local cache-key
/// discriminator — never persisted, logged, or compared against
/// attacker-controlled input. SHA-256 (rather than a `DefaultHasher`/SipHash
/// digest) is what makes two different secrets practically un-collidable, so
/// a rotated secret always misses the old cache entry instead of an attacker
/// — or bad luck — being able to alias a new secret onto a stale credential.
fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// Entra ID credentials, shared process-wide so a burst of new connections for
/// the same identity triggers one token acquisition instead of one per
/// connection (AB#46409). Each `azure_identity` credential already carries its
/// own per-scope, expiry-aware token cache internally, so sharing the
/// credential *instance* is enough to share tokens too — this map never
/// stores raw tokens itself, which keeps expiry/refresh entirely in the
/// already-tested Azure SDK code path.
///
/// This is a deliberate improvement beyond msodbcsql parity, not a gap versus
/// it: the classic C++ driver's `AzureADAuth` has no equivalent cache either —
/// `Parse.cpp:3655` constructs a stack-local `AzureADAuth auth;` and
/// re-authenticates on every connection for service principal and managed
/// identity. Only its interactive/WAM path caches (`MSQAAuthContextCache`),
/// mirrored here by the interactive path's own process-wide cache in
/// `msqa.rs`.
///
/// Unbounded like that interactive cache: entries are never evicted, so a
/// process that cycles through many distinct identities (e.g. a multi-tenant
/// service impersonating many different service principals) or rotates a
/// secret repeatedly retains one entry per identity/secret it has ever seen
/// for the life of the process. Acceptable for the same reason as the
/// interactive cache — realistic deployments use a small, stable set of
/// identities — but a candidate for follow-up if that stops holding.
static CREDENTIAL_CACHE: OnceLock<Mutex<HashMap<CredentialCacheKey, Arc<dyn TokenCredential>>>> =
    OnceLock::new();

/// Returns the cached credential for `key`, building and caching one via
/// `build` on a miss. Held across `build` deliberately: constructing a
/// credential is local config assembly (no I/O, no `.await`), so the lock is
/// never held across a blocking or network operation — the token request
/// itself always runs after this returns.
///
/// Logs a hit/miss at `debug` (identity kind only — never the tenant, client
/// id, or secret digest) so a connection storm's throttling behavior can be
/// correlated against actual cache effectiveness in production traces.
///
/// Recovers from a poisoned lock rather than propagating an error, unlike an
/// ODBC handle's `*.inner` mutex. That rule exists because a poisoned handle
/// mutex might guard a torn domain object, and erroring out affects only the
/// one handle that panicked — the application frees it and gets a fresh,
/// unpoisoned mutex on its next allocation. `CREDENTIAL_CACHE` is a `static`:
/// once poisoned it never recovers on its own, so treating poison as fatal
/// here would permanently fail every future connection's Entra auth for the
/// rest of the process from a single transient panic, instead of just the one
/// call in flight when it happened. The recovered map cannot be torn in a way
/// that matters either — the lock is never held across `build()`'s fallible
/// work in a way that leaves a partial insert (see above), so the worst case
/// is a missing entry, which is exactly a cache miss.
fn cached_credential(
    key: CredentialCacheKey,
    build: impl FnOnce() -> TdsResult<Arc<dyn TokenCredential>>,
) -> TdsResult<Arc<dyn TokenCredential>> {
    let cache = CREDENTIAL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|poisoned| {
        warn!("entra: credential cache mutex was poisoned by a prior panic; recovering its state rather than failing Entra auth process-wide");
        poisoned.into_inner()
    });

    if let Some(existing) = cache.get(&key) {
        debug!(kind = key.label(), "entra: reusing cached credential");
        return Ok(Arc::clone(existing));
    }

    debug!(
        kind = key.label(),
        "entra: credential cache miss, acquiring a new credential"
    );
    let credential = build()?;
    cache.insert(key, Arc::clone(&credential));
    Ok(credential)
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

/// An authentication method the driver cannot honour.
///
/// `requested` is what the connection string asked for and `resolved` is what
/// platform resolution turned it into. They differ only where a keyword maps to
/// a different method on this platform, and both are reported so the diagnostic
/// never names a keyword the user did not write.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UnsupportedAuth {
    pub(crate) requested: TdsAuthenticationMethod,
    pub(crate) resolved: TdsAuthenticationMethod,
}

impl UnsupportedAuth {
    /// The method was not resolved to anything else; it is simply unimplemented.
    fn plain(method: TdsAuthenticationMethod) -> Self {
        Self {
            requested: method.clone(),
            resolved: method,
        }
    }
}

/// Applies the resolved authentication to `context`: sets credentials for
/// SQL/SSPI, the pre-acquired token for `AccessToken`, or builds and registers
/// an Entra token factory for service principal / managed identity /
/// interactive. For the factory methods the credentials are captured by the
/// factory and left out of `context`, so they are never serialized in LOGIN7.
///
/// `server` labels the interactive sign-in window, so it is only read on
/// Windows, the sole platform with an interactive path.
///
/// Network-free: no token is acquired here. Returns the unsupported method as
/// `Err` so the caller can surface `HYC00`.
pub(crate) fn configure_auth(
    context: &mut ClientContext,
    resolved: TransformedAuth,
    #[cfg_attr(not(windows), allow(unused_variables))] server: &str,
) -> Result<(), UnsupportedAuth> {
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
        #[cfg(windows)]
        TdsAuthenticationMethod::ActiveDirectoryInteractive => {
            // A non-empty UID becomes the sign-in hint and the token-cache key;
            // no secret is stored in the context.
            let login_hint = (!resolved.user_name.is_empty()).then_some(resolved.user_name);
            // Sign-in involves a human and can take minutes, far longer than the
            // default 15s login deadline. Raise the overall login timeout while
            // leaving `connect_timeout` (the per-TCP-connect cap) at its default,
            // so an unreachable server still fails fast. An app-set
            // SQL_ATTR_LOGIN_TIMEOUT (already applied to the context) wins.
            // Mirrors msodbcsql's separate login vs. connection timeouts.
            if context.login_timeout.is_none() {
                context.login_timeout = Some(LOGIN_TIMEOUT_SECS);
            }
            let factory = InteractiveTokenFactory::new(login_hint, server.to_string());
            context.auth_method_map.insert(
                TdsAuthenticationMethod::ActiveDirectoryInteractive,
                Box::new(factory),
            );
        }
        // msodbcsql has no interactive path off Windows: `SNI_FedAuth` is not
        // compiled at all (its Makefile omits the translation unit) and the
        // dispatch site is removed by `#if !defined(XPLAT_ODBC_TODO)`
        // (`Parse.cpp:3597`). The request lands in the generic `AzureADAuth`
        // block, whose `authMode` ternary (`:3657-3660`) has no Interactive arm
        // and so resolves to `AKVCFG_AUTHMODE_INTEGRATED` — a Kerberos attempt
        // against the STS. Resolve it the same way; once Integrated is
        // implemented this becomes msodbcsql's behaviour exactly. The caller
        // still names the requested method, because parity justifies the
        // resolution, not a diagnostic about a keyword nobody typed.
        #[cfg(not(windows))]
        TdsAuthenticationMethod::ActiveDirectoryInteractive => {
            return Err(UnsupportedAuth {
                requested: TdsAuthenticationMethod::ActiveDirectoryInteractive,
                resolved: TdsAuthenticationMethod::ActiveDirectoryIntegrated,
            });
        }
        other => return Err(UnsupportedAuth::plain(other)),
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

    /// Applies `resolved` against a stand-in server name; only interactive
    /// sign-in reads it (for the window title).
    fn configure(
        ctx: &mut ClientContext,
        resolved: TransformedAuth,
    ) -> Result<(), UnsupportedAuth> {
        configure_auth(ctx, resolved, "testserver.database.windows.net")
    }

    #[test]
    fn configure_auth_service_principal_hides_credentials() {
        let mut ctx = ClientContext::default();
        let r = transformed(
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
            "client-id",
            "top-secret",
        );
        assert!(configure(&mut ctx, r).is_ok());
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
        assert!(configure(&mut ctx, r).is_ok());
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
        assert!(configure(&mut ctx, r).is_ok());
        assert_eq!(ctx.user_name, "sa");
        assert_eq!(ctx.password, "pw");
        assert!(ctx.auth_method_map.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn configure_auth_interactive_registers_factory() {
        let mut ctx = ClientContext::default();
        // UID is kept as the login hint; no secret is written to the context.
        let r = transformed(
            TdsAuthenticationMethod::ActiveDirectoryInteractive,
            "user@contoso.com",
            "",
        );
        assert!(configure(&mut ctx, r).is_ok());
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
        // Interactive raises the overall login deadline so a human has time to
        // sign in, while leaving the per-TCP-connect cap (`connect_timeout`) at
        // its default so an unreachable server still fails fast.
        assert_eq!(ctx.login_timeout, Some(LOGIN_TIMEOUT_SECS));
        const { assert!(LOGIN_TIMEOUT_SECS > 15) };
        assert_eq!(ctx.connect_timeout, 15);
    }

    #[cfg(windows)]
    #[test]
    fn configure_auth_interactive_preserves_app_login_timeout() {
        // An app-set SQL_ATTR_LOGIN_TIMEOUT (applied to the context before auth
        // config) must win over the interactive default.
        let mut ctx = ClientContext::default();
        ctx.login_timeout = Some(60);
        let r = transformed(
            TdsAuthenticationMethod::ActiveDirectoryInteractive,
            "user@contoso.com",
            "",
        );
        assert!(configure(&mut ctx, r).is_ok());
        assert_eq!(ctx.login_timeout, Some(60));
    }

    #[cfg(not(windows))]
    #[test]
    fn configure_auth_interactive_reports_integrated_off_windows() {
        // msodbcsql does not compile an interactive path off Windows; the
        // request falls through its `authMode` ternary (`Parse.cpp:3657-3660`)
        // to AKVCFG_AUTHMODE_INTEGRATED. Reporting Integrated keeps that
        // behaviour, and becomes a real Kerberos attempt once that method
        // lands.
        let mut ctx = ClientContext::default();
        let r = transformed(
            TdsAuthenticationMethod::ActiveDirectoryInteractive,
            "user@contoso.com",
            "",
        );
        assert_eq!(
            configure(&mut ctx, r),
            Err(UnsupportedAuth {
                requested: TdsAuthenticationMethod::ActiveDirectoryInteractive,
                resolved: TdsAuthenticationMethod::ActiveDirectoryIntegrated,
            }),
            "the error must still name the keyword the user wrote"
        );
        // Nothing is written to the context, and no factory is registered, so
        // the connection cannot proceed.
        assert!(ctx.auth_method_map.is_empty());
        assert!(ctx.login_timeout.is_none());
        assert_ne!(
            ctx.tds_authentication_method,
            TdsAuthenticationMethod::ActiveDirectoryInteractive
        );
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
            configure(&mut ctx, r),
            Err(UnsupportedAuth::plain(
                TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow
            )),
            "a method that resolves to itself reports itself"
        );
    }

    // --- Process-wide credential cache (AB#46409) ---

    use azure_core::credentials::{AccessToken, TokenRequestOptions};
    use azure_core::time::{Duration, OffsetDateTime};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A [`TokenCredential`] that counts calls and mints a distinct token each
    /// time, so a test can prove whether it was reused or rebuilt, and whether
    /// `get_token` was actually invoked (vs. a stale value cached above it).
    #[derive(Debug, Default)]
    struct CountingCredential {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TokenCredential for CountingCredential {
        async fn get_token(
            &self,
            _scopes: &[&str],
            _options: Option<TokenRequestOptions<'_>>,
        ) -> azure_core::Result<AccessToken> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(AccessToken::new(
                format!("token-{call}"),
                OffsetDateTime::now_utc() + Duration::seconds(3600),
            ))
        }
    }

    fn counting_credential() -> TdsResult<Arc<dyn TokenCredential>> {
        Ok(Arc::new(CountingCredential::default()))
    }

    #[test]
    fn service_principal_key_differs_by_tenant_client_and_secret() {
        let sts = "https://login.microsoftonline.com/tenant-a";
        let base = EntraTokenFactory::new(CredentialConfig::ServicePrincipalSecret {
            client_id: "cache-key-client".to_string(),
            secret: Secret::from("cache-key-secret-1".to_string()),
        });
        let same_again = EntraTokenFactory::new(CredentialConfig::ServicePrincipalSecret {
            client_id: "cache-key-client".to_string(),
            secret: Secret::from("cache-key-secret-1".to_string()),
        });
        let different_secret = EntraTokenFactory::new(CredentialConfig::ServicePrincipalSecret {
            client_id: "cache-key-client".to_string(),
            secret: Secret::from("cache-key-secret-2".to_string()),
        });
        let different_client = EntraTokenFactory::new(CredentialConfig::ServicePrincipalSecret {
            client_id: "cache-key-client-2".to_string(),
            secret: Secret::from("cache-key-secret-1".to_string()),
        });
        let different_tenant_sts = "https://login.microsoftonline.com/tenant-b";

        let base_key = base.cache_key(sts).unwrap();
        assert_eq!(base_key, same_again.cache_key(sts).unwrap());
        assert_ne!(
            base_key,
            different_secret.cache_key(sts).unwrap(),
            "different secrets must not share a cache entry"
        );
        assert_ne!(
            base_key,
            different_client.cache_key(sts).unwrap(),
            "different client ids must not share a cache entry"
        );
        assert_ne!(
            base_key,
            base.cache_key(different_tenant_sts).unwrap(),
            "different tenants must not share a cache entry"
        );
    }

    #[test]
    fn digest_is_deterministic_sha256_not_a_weak_hash() {
        // Pins the fix for a real review finding: a `DefaultHasher`/SipHash
        // digest is only DoS-resistant, not collision-resistant, so a
        // rotated secret could alias onto a stale cached credential. SHA-256
        // (32 bytes, deterministic, and distinguishing secrets that differ by
        // a single character) is what makes that practically impossible.
        assert_eq!(digest("same-secret"), digest("same-secret"));
        assert_ne!(digest("secret-a"), digest("secret-b"));
        assert_ne!(
            digest("secret"),
            digest("Secret"),
            "digest must be case-sensitive over the raw secret bytes"
        );
        assert_eq!(digest("x").len(), 32, "SHA-256 output is 32 bytes");
    }

    #[test]
    fn managed_identity_key_distinguishes_system_and_user_assigned() {
        let system = EntraTokenFactory::new(CredentialConfig::ManagedIdentity { client_id: None });
        let user_a = EntraTokenFactory::new(CredentialConfig::ManagedIdentity {
            client_id: Some("uid-cache-key-a".to_string()),
        });
        let user_b = EntraTokenFactory::new(CredentialConfig::ManagedIdentity {
            client_id: Some("uid-cache-key-b".to_string()),
        });

        let system_key = system.cache_key("unused").unwrap();
        let user_a_key = user_a.cache_key("unused").unwrap();
        assert_ne!(system_key, user_a_key);
        assert_ne!(user_a_key, user_b.cache_key("unused").unwrap());
        // The STS URL is ignored for managed identity: IMDS is machine-local.
        assert_eq!(
            system_key,
            system.cache_key("https://any.example/tenant").unwrap()
        );
    }

    #[test]
    fn cache_key_label_is_identity_free() {
        // The label feeds a `debug!` trace: it must say which auth method a
        // cache hit/miss was for without ever including the tenant, client
        // id, or secret digest.
        let sp = CredentialCacheKey::ServicePrincipal {
            authority_host: "https://login.microsoftonline.com".to_string(),
            tenant_id: "some-tenant".to_string(),
            client_id: "some-client".to_string(),
            secret_digest: digest("some-secret"),
        };
        let mi = CredentialCacheKey::ManagedIdentity {
            client_id: "some-client".to_string(),
        };
        assert_eq!(sp.label(), "service principal");
        assert_eq!(mi.label(), "managed identity");
        assert!(!sp.label().contains("some-tenant"));
        assert!(!sp.label().contains("some-client"));
    }

    #[test]
    fn cached_credential_reuses_the_same_instance_on_a_hit() {
        let key = CredentialCacheKey::ManagedIdentity {
            client_id: "cached-credential-hit".to_string(),
        };
        let build_calls = AtomicUsize::new(0);
        let build = || {
            build_calls.fetch_add(1, Ordering::SeqCst);
            counting_credential()
        };

        let first = cached_credential(key.clone(), build).unwrap();
        let second = cached_credential(key, build).unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "a cache hit must return the same credential instance"
        );
        assert_eq!(build_calls.load(Ordering::SeqCst), 1, "build must run once");
    }

    #[test]
    fn cached_credential_never_shares_across_distinct_identities() {
        let key_a = CredentialCacheKey::ManagedIdentity {
            client_id: "cached-credential-distinct-a".to_string(),
        };
        let key_b = CredentialCacheKey::ManagedIdentity {
            client_id: "cached-credential-distinct-b".to_string(),
        };

        let a = cached_credential(key_a, counting_credential).unwrap();
        let b = cached_credential(key_b, counting_credential).unwrap();

        assert!(
            !Arc::ptr_eq(&a, &b),
            "distinct identities must never share a credential"
        );
    }

    #[test]
    fn cached_credential_recovers_from_a_poisoned_lock() {
        // Runs in its own process under nextest, so poisoning the static
        // cache here cannot affect any other test.
        let key = CredentialCacheKey::ManagedIdentity {
            client_id: "cached-credential-poison-recovery".to_string(),
        };

        let cache = CREDENTIAL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = cache.lock().unwrap();
            panic!("deliberately poisoning the credential cache lock for this test");
        }));
        assert!(poisoned.is_err());
        assert!(cache.is_poisoned());

        // The reasoning on `cached_credential`: recovering here (instead of
        // propagating an error, as an ODBC handle mutex would) is what keeps
        // one unrelated panic from permanently failing Entra auth for the
        // rest of the process.
        let credential = cached_credential(key, counting_credential);
        assert!(
            credential.is_ok(),
            "poison recovery must not fail Entra auth process-wide"
        );
    }

    #[test]
    fn cached_credential_does_not_freeze_the_token_it_returns() {
        // Caching the *credential* must not accidentally cache the *token*:
        // each call through the shared instance should still reach the
        // delegate, which is what lets it refresh an expiring token. This is
        // the property that stands in for expiry here — actual expiry/refresh
        // timing is delegated to azure_identity's own tested token cache
        // (`azure_identity::cache::TokenCache`), not reimplemented locally.
        let key = CredentialCacheKey::ManagedIdentity {
            client_id: "cached-credential-refresh".to_string(),
        };
        let credential = cached_credential(key.clone(), counting_credential).unwrap();
        let same_credential =
            cached_credential(key, || panic!("must be a cache hit, not a rebuild")).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        let scopes = ["https://database.windows.net/.default"];
        let token1 = runtime
            .block_on(same_credential.get_token(&scopes, None))
            .expect("first token request succeeds");
        let token2 = runtime
            .block_on(credential.get_token(&scopes, None))
            .expect("second token request succeeds");

        assert_ne!(
            token1.token.secret(),
            token2.token.secret(),
            "each call must reach the shared credential rather than a frozen cached token"
        );
    }
}

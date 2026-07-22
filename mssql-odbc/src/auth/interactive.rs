// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ActiveDirectory Interactive (browser) authentication for the FedAuth
//! handshake (mssql-odbc T3).
//!
//! [`InteractiveTokenFactory`] implements the mssql-tds [`EntraIdTokenFactory`]
//! trait by hand-rolling the OAuth2 authorization-code flow with PKCE against
//! the Entra endpoints derived from the server's FEDAUTHINFO:
//!
//! 1. Bind a loopback listener on `127.0.0.1:0` for the redirect.
//! 2. Open the system browser at the `/authorize` endpoint (PKCE `S256`).
//! 3. Receive the redirect on the loopback listener and read the `code`.
//! 4. Exchange the `code` (with the PKCE verifier) at the `/token` endpoint.
//!
//! The Azure SDK (`azure_identity`) ships no interactive credential, so the flow
//! is implemented directly here. It uses the well-known msodbcsql / `SqlClient`
//! public-client id and an `http://localhost` loopback redirect, matching the
//! classic driver so no app registration is required.
//!
//! Security notes:
//! - PKCE (`S256`) binds the authorization code to this process; the `state`
//!   parameter is a single-use CSRF guard checked on the redirect.
//! - No client secret is used or stored (public client). The token is not
//!   cached: each login (including session recovery) runs a fresh sign-in, and
//!   no refresh token is retained. Token caching/refresh is tracked in AB#46409.
//! - The browser is launched even under `SQL_DRIVER_NOPROMPT`: that flag governs
//!   the ODBC DSN dialog, not the Entra sign-in, matching msodbcsql.
//! - The STS authority comes from the server's FEDAUTHINFO; like msodbcsql and
//!   the service-principal path in [`super::entra`], it is trusted as long as it
//!   is `https`. On a channel that is not certificate-validated
//!   (`TrustServerCertificate=yes`), a rogue server could point the sign-in at an
//!   attacker-controlled authority — use `Encrypt=Strict` or a validated server
//!   certificate for interactive auth.
//! - The loopback listener binds IPv4 `127.0.0.1` while the redirect advertises
//!   `http://localhost` (matching MSAL.NET); hosts that resolve `localhost` only
//!   to IPv6 `::1` are a known gap.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info};
use url::Url;

use super::entra::{encode_utf16le, normalize_scope, split_sts_url};
use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;

/// Well-known Microsoft public-client id used by msodbcsql / `SqlClient` for
/// ActiveDirectory Interactive. It is registered with an `http://localhost`
/// loopback redirect for native clients, so reusing it matches the classic
/// driver and needs no per-app registration.
const PUBLIC_CLIENT_ID: &str = "a94f9c62-97fe-4d19-b06d-472bed8d2bcb";

/// How long to wait for the user to finish signing in before giving up.
const REDIRECT_TIMEOUT: Duration = Duration::from_secs(300);

/// Login-connect budget applied to interactive connections via
/// `ClientContext.connect_timeout`. That single value bounds both the outer
/// login deadline and each TCP-connect attempt, so it must comfortably exceed
/// [`REDIRECT_TIMEOUT`]: the browser round-trip stays bounded while a normal
/// reachable server still connects in milliseconds. Mirrors SqlClient's
/// enlarged Connect Timeout for interactive auth. Splitting this into a separate
/// login timeout (`SQL_ATTR_LOGIN_TIMEOUT`) is a planned follow-up.
pub(super) const CONNECT_TIMEOUT_SECS: u32 = 330;

/// Number of random bytes for the PKCE verifier and the `state` value; 32 bytes
/// base64url-encode to a 43-character string (within the 43–128 PKCE range).
const RANDOM_BYTES: usize = 32;

/// Per-connection read timeout for a loopback callback, so a stalled local
/// client cannot hold the handler until [`REDIRECT_TIMEOUT`].
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on bytes read while looking for the HTTP request line.
const MAX_REQUEST_BYTES: usize = 8192;

/// Acquires an Entra ID access token via the interactive (browser) flow during
/// the FedAuth handshake.
#[derive(Clone)]
pub(crate) struct InteractiveTokenFactory {
    /// Optional `login_hint` (the ODBC `UID`, typically `user@tenant`).
    login_hint: Option<String>,
}

impl InteractiveTokenFactory {
    pub(crate) fn new(login_hint: Option<String>) -> Self {
        Self { login_hint }
    }
}

#[async_trait]
impl EntraIdTokenFactory for InteractiveTokenFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        // No token cache: like the service-principal path, each login runs the
        // flow fresh so session recovery cannot reuse an expired token. Token
        // caching/refresh is tracked in AB#46409.
        let (authority, tenant) = split_sts_url(&sts_url)?;
        let scope = normalize_scope(&spn);
        let token = acquire_interactive_token(
            &authority,
            &tenant,
            PUBLIC_CLIENT_ID,
            &scope,
            self.login_hint.as_deref(),
        )
        .await?;

        Ok(encode_utf16le(&token))
    }
}

/// A PKCE verifier and its derived `S256` challenge (RFC 7636).
struct Pkce {
    verifier: String,
    challenge: String,
}

/// Returns `count` CSPRNG bytes base64url-encoded (no padding) — used for the
/// PKCE verifier and the `state` value.
fn random_base64url(count: usize) -> TdsResult<String> {
    let mut buf = vec![0u8; count];
    getrandom::fill(&mut buf)
        .map_err(|e| Error::ConnectionError(format!("failed to generate random bytes: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(&buf))
}

/// Computes the PKCE `S256` challenge: `base64url(SHA256(verifier))`.
fn pkce_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn generate_pkce() -> TdsResult<Pkce> {
    let verifier = random_base64url(RANDOM_BYTES)?;
    let challenge = pkce_challenge(&verifier);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

/// Inputs for the `/authorize` request URL.
struct AuthorizeRequest<'a> {
    authority: &'a str,
    tenant: &'a str,
    client_id: &'a str,
    scope: &'a str,
    redirect_uri: &'a str,
    state: &'a str,
    challenge: &'a str,
    login_hint: Option<&'a str>,
}

/// Builds the `/authorize` request URL for the authorization-code + PKCE flow.
fn build_authorize_url(request: &AuthorizeRequest<'_>) -> TdsResult<Url> {
    let AuthorizeRequest {
        authority,
        tenant,
        client_id,
        scope,
        redirect_uri,
        state,
        challenge,
        login_hint,
    } = *request;
    let mut url = Url::parse(&format!("{authority}/{tenant}/oauth2/v2.0/authorize"))
        .map_err(|e| Error::ConnectionError(format!("invalid authorize endpoint: {e}")))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("client_id", client_id);
        query.append_pair("response_type", "code");
        query.append_pair("redirect_uri", redirect_uri);
        query.append_pair("response_mode", "query");
        query.append_pair("scope", scope);
        query.append_pair("state", state);
        query.append_pair("code_challenge", challenge);
        query.append_pair("code_challenge_method", "S256");
        if let Some(hint) = login_hint.filter(|h| !h.is_empty()) {
            query.append_pair("login_hint", hint);
        }
    }
    Ok(url)
}

fn token_endpoint(authority: &str, tenant: &str) -> String {
    format!("{authority}/{tenant}/oauth2/v2.0/token")
}

/// Runs the full interactive flow and returns the raw access token.
async fn acquire_interactive_token(
    authority: &str,
    tenant: &str,
    client_id: &str,
    scope: &str,
    login_hint: Option<&str>,
) -> TdsResult<String> {
    let pkce = generate_pkce()?;
    let state = random_base64url(RANDOM_BYTES)?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| Error::ConnectionError(format!("failed to bind loopback listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::ConnectionError(format!("failed to read listener address: {e}")))?
        .port();
    // Loopback redirect per RFC 8252; the well-known client allows any port.
    let redirect_uri = format!("http://localhost:{port}");

    let authorize_url = build_authorize_url(&AuthorizeRequest {
        authority,
        tenant,
        client_id,
        scope,
        redirect_uri: &redirect_uri,
        state: &state,
        challenge: &pkce.challenge,
        login_hint,
    })?;

    info!("launching browser for interactive Entra sign-in");
    debug!(%authorize_url, "interactive authorize URL");
    open_browser(authorize_url.as_str()).map_err(|e| {
        Error::ConnectionError(format!(
            "failed to launch a browser for interactive sign-in: {e}"
        ))
    })?;

    let code = tokio::time::timeout(REDIRECT_TIMEOUT, wait_for_redirect(&listener, &state))
        .await
        .map_err(|_| {
            Error::ConnectionError("timed out waiting for interactive sign-in".into())
        })??;

    exchange_code_for_token(
        authority,
        tenant,
        client_id,
        scope,
        &redirect_uri,
        &code,
        &pkce.verifier,
    )
    .await
}

/// Accepts loopback connections until the OAuth redirect with a matching `state`
/// arrives, replies with a small HTML page, and returns the authorization code.
/// Unrelated or unmatched callbacks (stray local requests, `favicon.ico`) are
/// answered and ignored so they cannot abort or hijack the sign-in.
async fn wait_for_redirect(listener: &TcpListener, expected_state: &str) -> TdsResult<String> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| {
            Error::ConnectionError(format!("failed to accept redirect connection: {e}"))
        })?;

        let outcome = read_request_query(&mut stream)
            .await
            .as_deref()
            .map_or(RedirectOutcome::Unrelated, |query| {
                classify_redirect(query, expected_state)
            });

        match outcome {
            RedirectOutcome::Code(code) => {
                write_http_response(
                    &mut stream,
                    "200 OK",
                    "Sign-in complete. You can close this window and return to your application.",
                )
                .await;
                return Ok(code);
            }
            RedirectOutcome::Failed(detail) => {
                write_http_response(
                    &mut stream,
                    "400 Bad Request",
                    "Sign-in failed. You can close this window and return to your application.",
                )
                .await;
                return Err(Error::ConnectionError(format!(
                    "interactive sign-in failed: {detail}"
                )));
            }
            RedirectOutcome::Unrelated => {
                write_http_response(&mut stream, "404 Not Found", "").await;
            }
        }
    }
}

/// Reads an incoming request up to the end of the request line (first CRLF),
/// bounded by [`READ_TIMEOUT`] and [`MAX_REQUEST_BYTES`], and returns its query
/// string. Returns `None` on timeout, EOF, error, or a request with no query.
async fn read_request_query(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => break, // EOF, read error, or timeout
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..end]);
            return request_target_query(&line);
        }
        if buf.len() > MAX_REQUEST_BYTES {
            break;
        }
    }
    None
}

async fn write_http_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Extracts the query string from an HTTP request line such as
/// `GET /?code=...&state=... HTTP/1.1`.
fn request_target_query(request_line: &str) -> Option<String> {
    let target = request_line.split_whitespace().nth(1)?;
    let (_, query) = target.split_once('?')?;
    Some(query.to_string())
}

/// The meaningful outcome of a loopback callback.
enum RedirectOutcome {
    /// Authorization code from a callback whose `state` matched.
    Code(String),
    /// Server-reported error from a callback whose `state` matched.
    Failed(String),
    /// No matching `state` or nothing actionable: ignore and keep waiting.
    Unrelated,
}

/// Classifies a redirect query. A matching `state` is required before honoring
/// either a `code` or an `error`, so an unauthenticated local callback can
/// neither inject a code nor abort the sign-in.
fn classify_redirect(query: &str, expected_state: &str) -> RedirectOutcome {
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };

    if get("state").as_deref() != Some(expected_state) {
        return RedirectOutcome::Unrelated;
    }
    if let Some(code) = get("code").filter(|c| !c.is_empty()) {
        return RedirectOutcome::Code(code);
    }
    if let Some(error) = get("error") {
        let description = get("error_description").unwrap_or_default();
        return RedirectOutcome::Failed(format!("{error}: {description}"));
    }
    RedirectOutcome::Unrelated
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: String,
}

/// Exchanges the authorization code (plus the PKCE verifier) for an access token
/// at the `/token` endpoint. Public client: no secret is sent.
async fn exchange_code_for_token(
    authority: &str,
    tenant: &str,
    client_id: &str,
    scope: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> TdsResult<String> {
    let endpoint = token_endpoint(authority, tenant);
    let form = [
        ("client_id", client_id),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("scope", scope),
    ];
    // Build the urlencoded body directly: `RequestBuilder::form` is not compiled
    // into our `default-features = false` reqwest, and `url` is already a dep.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().copied())
        .finish();

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| Error::ConnectionError(format!("failed to build HTTP client: {e}")))?;
    let response = client
        .post(&endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| Error::ConnectionError(format!("token request failed: {e}")))?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| Error::ConnectionError(format!("failed to read token response: {e}")))?;
    let body = String::from_utf8_lossy(&bytes);

    if !status.is_success() {
        let detail = serde_json::from_str::<TokenErrorResponse>(body.as_ref())
            .map(|e| format!("{}: {}", e.error, e.error_description))
            .unwrap_or_else(|_| body.into_owned());
        return Err(Error::ConnectionError(format!(
            "interactive token exchange failed ({status}): {detail}"
        )));
    }

    parse_token_response(body.as_ref())
}

fn parse_token_response(body: &str) -> TdsResult<String> {
    let token = serde_json::from_str::<TokenResponse>(body)
        .map(|token| token.access_token)
        .map_err(|e| Error::ConnectionError(format!("failed to parse token response: {e}")))?;
    if token.is_empty() {
        return Err(Error::ConnectionError(
            "token response contained an empty access token".into(),
        ));
    }
    Ok(token)
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> std::io::Result<()> {
    // Launch via explorer.exe (no shell): the percent-encoded URL is handed to
    // the default browser verbatim. Routing through `cmd /C start` would let cmd
    // expand `%..%` sequences (corrupting the URL, which contains `%XX`) and
    // parse `&`, and would risk command injection from the server-provided
    // authority.
    std::process::Command::new("explorer.exe")
        .arg(url)
        .spawn()
        .map(|_| ())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> std::io::Result<()> {
    reap(std::process::Command::new("open").arg(url).spawn()?);
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_browser(url: &str) -> std::io::Result<()> {
    reap(std::process::Command::new("xdg-open").arg(url).spawn()?);
    Ok(())
}

/// `open`/`xdg-open` exit as soon as they hand the URL to the browser, but a
/// dropped `Child` is never waited on and would linger as a zombie. Reap it on a
/// detached thread so the sign-in flow is not blocked.
#[cfg(unix)]
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 Appendix B worked example.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_base64url_is_url_safe_and_sized() {
        let value = random_base64url(RANDOM_BYTES).unwrap();
        // 32 bytes -> 43 base64 chars with no padding.
        assert_eq!(value.len(), 43);
        assert!(!value.contains(['+', '/', '=']));
    }

    #[test]
    fn generate_pkce_challenge_derives_from_verifier() {
        let pkce = generate_pkce().unwrap();
        assert_eq!(pkce.challenge, pkce_challenge(&pkce.verifier));
    }

    #[test]
    fn authorize_url_has_expected_parameters() {
        let url = build_authorize_url(&AuthorizeRequest {
            authority: "https://login.microsoftonline.com",
            tenant: "my-tenant",
            client_id: "client-123",
            scope: "https://database.windows.net/.default",
            redirect_uri: "http://localhost:54321",
            state: "state-abc",
            challenge: "challenge-xyz",
            login_hint: Some("user@contoso.com"),
        })
        .unwrap();

        assert_eq!(
            url.path(),
            "/my-tenant/oauth2/v2.0/authorize",
            "unexpected authorize path"
        );
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], "client-123");
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["redirect_uri"], "http://localhost:54321");
        assert_eq!(pairs["response_mode"], "query");
        assert_eq!(pairs["scope"], "https://database.windows.net/.default");
        assert_eq!(pairs["state"], "state-abc");
        assert_eq!(pairs["code_challenge"], "challenge-xyz");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["login_hint"], "user@contoso.com");
    }

    #[test]
    fn authorize_url_omits_empty_login_hint() {
        let url = build_authorize_url(&AuthorizeRequest {
            authority: "https://login.microsoftonline.com",
            tenant: "my-tenant",
            client_id: "client-123",
            scope: "scope",
            redirect_uri: "http://localhost:1",
            state: "s",
            challenge: "c",
            login_hint: None,
        })
        .unwrap();
        assert!(!url.query().unwrap_or_default().contains("login_hint"));
    }

    #[test]
    fn token_endpoint_is_v2() {
        assert_eq!(
            token_endpoint("https://login.microsoftonline.com", "my-tenant"),
            "https://login.microsoftonline.com/my-tenant/oauth2/v2.0/token"
        );
    }

    #[test]
    fn request_target_query_extracts_query() {
        assert_eq!(
            request_target_query("GET /?code=abc&state=xyz HTTP/1.1").as_deref(),
            Some("code=abc&state=xyz")
        );
    }

    #[test]
    fn request_target_query_without_query_is_none() {
        assert_eq!(request_target_query("GET /favicon.ico HTTP/1.1"), None);
        assert_eq!(request_target_query(""), None);
    }

    #[test]
    fn classify_redirect_returns_code_on_state_match() {
        match classify_redirect("code=the-code&state=st", "st") {
            RedirectOutcome::Code(code) => assert_eq!(code, "the-code"),
            _ => panic!("expected Code"),
        }
    }

    #[test]
    fn classify_redirect_ignores_state_mismatch_even_with_code() {
        // A forged local callback with the wrong state must not yield a code.
        assert!(matches!(
            classify_redirect("code=c&state=wrong", "expected"),
            RedirectOutcome::Unrelated
        ));
    }

    #[test]
    fn classify_redirect_ignores_missing_state() {
        assert!(matches!(
            classify_redirect("code=c", "st"),
            RedirectOutcome::Unrelated
        ));
    }

    #[test]
    fn classify_redirect_surfaces_server_error_on_state_match() {
        match classify_redirect(
            "error=access_denied&error_description=user+cancelled&state=st",
            "st",
        ) {
            RedirectOutcome::Failed(detail) => {
                assert!(detail.contains("access_denied"), "detail: {detail}");
                assert!(detail.contains("user cancelled"), "detail: {detail}");
            }
            _ => panic!("expected Failed"),
        }
    }

    #[test]
    fn classify_redirect_ignores_error_without_state_match() {
        // An error callback with no matching state must not abort the sign-in.
        assert!(matches!(
            classify_redirect("error=access_denied", "st"),
            RedirectOutcome::Unrelated
        ));
    }

    #[test]
    fn parse_token_response_extracts_access_token() {
        let body = r#"{"token_type":"Bearer","expires_in":3599,"access_token":"the.jwt.token"}"#;
        assert_eq!(parse_token_response(body).unwrap(), "the.jwt.token");
    }

    #[test]
    fn parse_token_response_rejects_invalid_json() {
        assert!(parse_token_response("not json").is_err());
    }

    #[test]
    fn parse_token_response_rejects_empty_access_token() {
        assert!(parse_token_response(r#"{"access_token":""}"#).is_err());
    }
}

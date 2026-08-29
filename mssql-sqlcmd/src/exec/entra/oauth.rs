// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The two OAuth2 flows `azure_identity` does not provide.
//!
//! Both talk to the tenant's token endpoint directly. The endpoint is derived
//! from the STS URL the server named in FEDAUTHINFO, which is required to be
//! `https` before anything is sent to it.

use azure_core::credentials::{Secret, TokenCredential};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;
use std::time::Duration;

/// The client id `sqlcmd` presents for the flows that need a public client.
///
/// This is the well-known Microsoft SQL tooling application, the same one the
/// reference implementations use, so a tenant that already permits `sqlcmd`
/// does not need a new app registration.
const SQL_TOOLING_CLIENT_ID: &str = "a94f9c62-97fe-4d19-b06d-472bed8d2bcf";

/// A client assertion that is already signed, supplied on the command line.
///
/// `ClientAssertionCredential` wants something it can call repeatedly; here the
/// value is fixed, so every call returns the same string.
#[derive(Debug, Clone)]
pub struct StaticAssertion(pub String);

#[async_trait::async_trait]
impl azure_identity::ClientAssertion for StaticAssertion {
    async fn secret(
        &self,
        _options: Option<azure_core::http::ClientMethodOptions<'_>>,
    ) -> azure_core::Result<String> {
        Ok(self.0.clone())
    }
}

/// Builds the v2 token endpoint for the tenant the STS URL names.
fn token_endpoint(sts_url: &str) -> TdsResult<String> {
    let url = url::Url::parse(sts_url.trim())
        .map_err(|e| Error::ConnectionError(format!("invalid STS URL: {sts_url} ({e})")))?;
    if url.scheme() != "https" {
        return Err(Error::ConnectionError(format!(
            "STS URL must use https: {sts_url}"
        )));
    }
    let authority = &url[..url::Position::BeforePath];
    let tenant = url
        .path_segments()
        .and_then(|mut s| s.next())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::ConnectionError(format!("STS URL is missing a tenant: {sts_url}")))?;
    Ok(format!("{authority}/{tenant}/oauth2/v2.0/token"))
}

/// Reads `access_token` out of a token response, or turns the OAuth error into
/// a message.
fn access_token(body: &serde_json::Value) -> Result<String, String> {
    if let Some(token) = body.get("access_token").and_then(|t| t.as_str()) {
        return Ok(token.to_string());
    }
    let code = body
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("unknown_error");
    let description = body
        .get("error_description")
        .and_then(|d| d.as_str())
        .unwrap_or("no description");
    Err(format!("{code}: {description}"))
}

/// Resource-owner password credentials.
///
/// Microsoft discourages this flow and many tenants block it outright — it
/// cannot satisfy conditional access or MFA. It is here because both reference
/// implementations accept `ActiveDirectoryPassword`, and refusing it would be a
/// gap rather than a safeguard. The failure, when a tenant does block it, comes
/// back from the endpoint with an explanation.
pub async fn password_token(
    sts_url: &str,
    user: &str,
    password: &str,
    scope: &str,
) -> TdsResult<String> {
    let endpoint = token_endpoint(sts_url)?;
    let response = reqwest::Client::new()
        .post(&endpoint)
        .form(&[
            ("client_id", SQL_TOOLING_CLIENT_ID),
            ("grant_type", "password"),
            ("username", user),
            ("password", password),
            ("scope", scope),
        ])
        .send()
        .await
        .map_err(|e| Error::ConnectionError(format!("token request failed: {e}")))?;

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| Error::ConnectionError(format!("malformed token response: {e}")))?;

    access_token(&body).map_err(|e| Error::ConnectionError(format!("password sign-in failed: {e}")))
}

/// The device code flow: print a code, then poll until the user has signed in
/// on another device.
///
/// This is the flow that suits a headless or remote session, where no browser
/// can be opened locally.
pub async fn device_code_token(
    sts_url: &str,
    client_id: &str,
    scope: &str,
) -> Result<String, String> {
    let endpoint = token_endpoint(sts_url).map_err(|e| e.to_string())?;
    let device_endpoint = endpoint.replace("/token", "/devicecode");
    let client_id = if client_id.is_empty() {
        SQL_TOOLING_CLIENT_ID
    } else {
        client_id
    };

    let http = reqwest::Client::new();
    let start: serde_json::Value = http
        .post(&device_endpoint)
        .form(&[("client_id", client_id), ("scope", scope)])
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("malformed device code response: {e}"))?;

    let device_code = start
        .get("device_code")
        .and_then(|c| c.as_str())
        .ok_or_else(|| match access_token(&start) {
            Ok(_) => "device code response carried no device_code".to_string(),
            Err(e) => e,
        })?;

    // The service supplies the words to show the user; printing its own message
    // keeps the instructions correct as the sign-in page changes.
    if let Some(message) = start.get("message").and_then(|m| m.as_str()) {
        eprintln!("{message}");
    }

    let mut interval = Duration::from_secs(
        start
            .get("interval")
            .and_then(|i| i.as_u64())
            .unwrap_or(5)
            .clamp(1, 60),
    );
    let expires_in = start
        .get("expires_in")
        .and_then(|e| e.as_u64())
        .unwrap_or(900);
    let deadline = std::time::Instant::now() + Duration::from_secs(expires_in);

    while std::time::Instant::now() < deadline {
        tokio::time::sleep(interval).await;

        let body: serde_json::Value = http
            .post(&endpoint)
            .form(&[
                ("client_id", client_id),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
            ])
            .send()
            .await
            .map_err(|e| format!("token poll failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("malformed token response: {e}"))?;

        match access_token(&body) {
            Ok(token) => return Ok(token),
            Err(e) => {
                let code = body.get("error").and_then(|c| c.as_str()).unwrap_or("");
                match code {
                    // Still waiting for the user; keep polling.
                    "authorization_pending" => {}
                    // Polling too fast: the service asks for a longer gap.
                    "slow_down" => interval += Duration::from_secs(5),
                    _ => return Err(e),
                }
            }
        }
    }

    Err("timed out waiting for sign-in".to_string())
}

/// Keeps the unused-import checker honest about the traits this module needs.
#[allow(dead_code)]
fn _assert_traits(c: &dyn TokenCredential, _s: &Secret) {
    let _ = c;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_endpoint_is_derived_from_the_sts_url() {
        assert_eq!(
            token_endpoint("https://login.microsoftonline.com/contoso").unwrap(),
            "https://login.microsoftonline.com/contoso/oauth2/v2.0/token"
        );
    }

    #[test]
    fn a_plaintext_sts_url_is_refused() {
        // The password is posted to this endpoint.
        assert!(token_endpoint("http://login.microsoftonline.com/contoso").is_err());
    }

    #[test]
    fn an_sts_url_without_a_tenant_is_refused() {
        assert!(token_endpoint("https://login.microsoftonline.com/").is_err());
    }

    #[test]
    fn an_access_token_is_read_out_of_a_successful_response() {
        let body = serde_json::json!({ "access_token": "abc", "expires_in": 3600 });
        assert_eq!(access_token(&body).unwrap(), "abc");
    }

    #[test]
    fn an_oauth_error_keeps_its_code_and_description() {
        let body = serde_json::json!({
            "error": "invalid_grant",
            "error_description": "AADSTS50126: bad credentials",
        });
        let message = access_token(&body).unwrap_err();
        assert!(message.contains("invalid_grant"), "{message}");
        assert!(message.contains("AADSTS50126"), "{message}");
    }

    #[test]
    fn a_response_that_is_neither_still_reports_something() {
        let message = access_token(&serde_json::json!({})).unwrap_err();
        assert!(message.contains("unknown_error"), "{message}");
    }

    #[tokio::test]
    async fn a_static_assertion_returns_the_value_it_was_given() {
        let assertion = StaticAssertion("signed-jwt".to_string());
        assert_eq!(
            azure_identity::ClientAssertion::secret(&assertion, None)
                .await
                .unwrap(),
            "signed-jwt"
        );
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `Authentication=ActiveDirectoryIntegrated` off Windows: an Entra token for
//! the Kerberos identity in the default credential cache, obtained through the
//! tenant's ADFS federation.
//!
//! A port of msodbcsql's Unix flow (`AzureADAuth.cpp`, `CheckFederated` and
//! `GetAccessToken`):
//!
//! 1. Read the principal from the default GSSAPI credential (`kinit`).
//! 2. `GET {sts-host}/common/UserRealm/{user}` to confirm the account is
//!    federated and find its ADFS metadata exchange (MEX) document.
//! 3. Pick the WS-Trust endpoint MEX advertises for HTTP Negotiate, preferring
//!    WS-Trust 1.3 over 2005.
//! 4. POST a WS-Trust `RequestSecurityToken` to it, authenticated with SPNEGO
//!    (RFC 4559), and take the SAML assertion from the response.
//! 5. Exchange the assertion at `{sts}/oauth2/token` for an access token using
//!    the SAML-bearer grant.
//!
//! Every URL must be `https`, matching msodbcsql, which restricts curl to
//! `CURLPROTO_HTTPS` for these requests. Redirects are not followed.
//!
//! The HTTP transport and the Kerberos identity are traits so the whole flow is
//! unit-tested on every platform against scripted responses; the production
//! implementations are reqwest and mssql-tds's GSSAPI bindings.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;
use mssql_tds::security::{SecurityContext, SecurityError};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tracing::debug;

/// msodbcsql's Entra application id, spelled as `AzureADAuth.cpp:688` sends it.
const CLIENT_ID: &str = "2C1229AA-16C5-4FF5-B46B-4F7FE2A2A9C8";

/// `cloud_audience_urn` is optional in the realm response; this is the value
/// msodbcsql falls back to.
const DEFAULT_CLOUD_AUDIENCE: &str = "urn:federation:MicrosoftOnline";

/// `apiversions[0]` in `AzureADAuth.cpp`, appended to the token endpoint.
const TOKEN_API_VERSION: &str = "?api-version=2015-06-01";

const WST13_ISSUE: &str = "http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue";
const WST13_NAMESPACE: &str = "http://docs.oasis-open.org/ws-sx/ws-trust/200512";
const WST13_BEARER_KEY: &str = "http://docs.oasis-open.org/ws-sx/ws-trust/200512/Bearer";
const WST13_REQUEST_ISSUE: &str = "http://docs.oasis-open.org/ws-sx/ws-trust/200512/Issue";
const WST2005_ISSUE: &str = "http://schemas.xmlsoap.org/ws/2005/02/trust/RST/Issue";
const WST2005_NAMESPACE: &str = "http://schemas.xmlsoap.org/ws/2005/02/trust";
const WST2005_NO_PROOF_KEY: &str = "http://schemas.xmlsoap.org/ws/2005/05/identity/NoProofKey";
const WST2005_REQUEST_ISSUE: &str = "http://schemas.xmlsoap.org/ws/2005/02/trust/Issue";

const SAML2_NAMESPACE: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
/// Opening and closing tags of a SAML 2.0 and a SAML 1.1 assertion, in the
/// order msodbcsql looks for them.
const SAML_ASSERTION_TAGS: [(&str, &str); 2] = [
    ("<saml:Assertion", "</saml:Assertion>"),
    ("<saml1:Assertion", "</saml1:Assertion>"),
];

const SOAP_CONTENT_TYPE: &str = "application/soap+xml; charset=utf-8";
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const SOAP_FAULT_REASON: (&str, &str) = ("<s:Reason>", "</s:Reason>");
const NEGOTIATE: &str = "Negotiate";

/// The markers `AzureADAuth.cpp:461-535` scans the MEX document for.
mod mex_tag {
    pub(super) const POLICY_ID: &str = "<wsp:Policy wsu:Id=";
    pub(super) const POLICY_END: &str = "</wsp:ExactlyOne>";
    pub(super) const NEGOTIATE_ASSERTION: &str = "<http:NegotiateAuthentication";
    pub(super) const BINDING_NAME: &str = "<wsdl:binding name=";
    pub(super) const BINDING_END: &str = "</wsdl:binding>";
    pub(super) const POLICY_REFERENCE: &str = "<wsp:PolicyReference URI=";
    pub(super) const PORT_NAME: &str = "<wsdl:port name=";
    pub(super) const PORT_BINDING: &str = "<wsdl:port binding=";
    pub(super) const PORT_BINDING_ATTRIBUTE: &str = "binding=";
    pub(super) const PORT_END: &str = "</wsdl:port>";
    pub(super) const ADDRESS: &str = "<wsa10:Address>";
    pub(super) const ADDRESS_END: &str = "</wsa10:Address>";
}

/// Bounds a Negotiate exchange that keeps answering 401 with a new token.
const MAX_NEGOTIATE_ROUNDS: usize = 5;

/// Longest response body quoted in an error message. msodbcsql quotes bodies
/// whole, but an ADFS error page can run to many kilobytes.
const MAX_QUOTED_BODY: usize = 2048;

const LABEL: &str = "Entra integrated authentication failed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HttpMethod {
    Get,
    Post,
}

#[derive(Clone, Debug)]
pub(super) struct HttpRequest {
    pub(super) method: HttpMethod,
    pub(super) url: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Option<String>,
}

impl HttpRequest {
    fn get(url: String) -> Self {
        Self {
            method: HttpMethod::Get,
            url,
            headers: Vec::new(),
            body: None,
        }
    }

    fn post(url: String, headers: Vec<(String, String)>, body: String) -> Self {
        Self {
            method: HttpMethod::Post,
            url,
            headers,
            body: Some(body),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct HttpResponse {
    pub(super) status: u16,
    /// Every `WWW-Authenticate` header, in order.
    pub(super) www_authenticate: Vec<String>,
    pub(super) body: String,
}

#[async_trait]
pub(super) trait HttpTransport: Send + Sync {
    /// Sends one request. `Err` is reserved for transport failures; any HTTP
    /// status, including errors, is returned as a response.
    async fn send(&self, request: &HttpRequest) -> TdsResult<HttpResponse>;
}

/// The Kerberos identity the flow authenticates as. Both calls may block.
pub(super) trait KerberosIdentity: Send + Sync {
    /// The display name of the default credential, e.g. `user@REALM`.
    fn principal(&self) -> TdsResult<String>;

    /// A fresh SPNEGO context targeting `HTTP@host`.
    fn negotiate_context(&self, host: &str) -> TdsResult<Box<dyn SecurityContext>>;
}

/// msodbcsql's `Request` retry policy (`AzureADAuth.cpp:144-323`): any status
/// other than 200 or 401 is retried up to three times within five seconds,
/// doubling a 500ms wait. Transport failures are not retried.
#[derive(Clone, Copy, Debug)]
pub(super) struct RetryPolicy {
    max_retries: u32,
    timeout: Duration,
    initial_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            timeout: Duration::from_millis(5000),
            initial_wait: Duration::from_millis(500),
        }
    }
}

/// Acquires an access token for `resource` from the Entra authority `sts_url`
/// on behalf of the identity's Kerberos principal.
pub(super) async fn acquire_token(
    transport: &dyn HttpTransport,
    identity: Arc<dyn KerberosIdentity>,
    sts_url: &str,
    resource: &str,
    retry: RetryPolicy,
) -> TdsResult<String> {
    if !sts_url.starts_with("https://") {
        return Err(not_https());
    }
    let host_end = sts_host_end(sts_url)
        .ok_or_else(|| failure("the authentication endpoint URL has no path".to_string()))?;

    let principal = blocking(Arc::clone(&identity), |id| id.principal()).await?;
    debug!("integrated: resolving the Entra user realm");

    let realm_url = format!(
        "{}common/UserRealm/{}?api-version=1.0",
        &sts_url[..=host_end],
        realm_user(&principal)
    );
    let realm = send_with_retry(transport, None, HttpRequest::get(realm_url), retry).await?;
    if realm.status != 200 {
        return Err(failure(format!(
            "Error getting realm info\n{}",
            quote(&realm.body)
        )));
    }
    let realm_info = parse_realm(&realm.body)?;

    let mex = send_with_retry(
        transport,
        None,
        HttpRequest::get(realm_info.federation_metadata_url),
        retry,
    )
    .await?;
    if mex.status != 200 {
        return Err(failure(format!("Error getting MEX\n{}", quote(&mex.body))));
    }
    let (endpoint, wst13) = select_endpoint(&mex.body)?;
    debug!(endpoint = %endpoint, wst13, "integrated: requesting a SAML assertion from ADFS");

    let action = if wst13 { WST13_ISSUE } else { WST2005_ISSUE };
    let envelope = soap_request(
        &endpoint,
        wst13,
        &realm_info.cloud_audience_urn,
        &uuid::Uuid::new_v4().hyphenated().to_string(),
    );
    let soap = HttpRequest::post(
        endpoint,
        vec![
            ("Content-Type".to_string(), SOAP_CONTENT_TYPE.to_string()),
            ("SOAPAction".to_string(), action.to_string()),
        ],
        envelope,
    );
    let saml_response = send_with_retry(transport, Some(&identity), soap, retry).await?;
    if saml_response.status != 200 {
        return Err(failure(format!(
            "{}\nError requesting SAML token.",
            quote(&saml_response.body)
        )));
    }
    // Unlike the other error paths, this 200 response may hold a bearer
    // assertion under a prefix we do not recognise, so its body is withheld.
    let assertion = extract_assertion(&saml_response.body).ok_or_else(|| {
        failure(format!(
            "SAML token not found in response ({} bytes).",
            saml_response.body.len()
        ))
    })?;

    let token_request = HttpRequest::post(
        token_url(sts_url),
        vec![("Content-Type".to_string(), FORM_CONTENT_TYPE.to_string())],
        token_request_body(resource, assertion),
    );
    let token = send_with_retry(transport, None, token_request, retry).await?;
    if token.status != 200 {
        return Err(failure(token_error_description(&token.body)));
    }
    parse_token_response(&token.body)
}

/// Returns the index of the `/` that ends the authority of an `https://` URL,
/// or `None` when the URL is not `https` or has no path
/// (`AzureADAuth.cpp:374`).
fn sts_host_end(sts_url: &str) -> Option<usize> {
    const SCHEME: &str = "https://";
    if !sts_url.starts_with(SCHEME) {
        return None;
    }
    sts_url[SCHEME.len()..].find('/').map(|i| i + SCHEME.len())
}

fn not_https() -> Error {
    failure("the authentication endpoint must use https".to_string())
}

/// The user segment of the realm URL (`AzureADAuth.cpp:419-431`). A default
/// credential of `user@alt@REALM` or `user\@alt@REALM` looks the account up
/// under `alt`, since the Kerberos realm need not match the Entra domain.
fn realm_user(principal: &str) -> String {
    if let Some(at1) = principal.find('@')
        && let Some(offset) = principal[at1 + 1..].find('@')
    {
        let at2 = at1 + 1 + offset;
        let user_end = if principal[..at1].ends_with('\\') {
            at1 - 1
        } else {
            at1
        };
        let mut user = url_encode(&principal[..user_end]);
        user.push_str(&url_encode(&principal[at1..at2]));
        return user;
    }
    url_encode(principal)
}

/// RFC 3986 unreserved characters, the only bytes msodbcsql's `URLencode`
/// leaves unescaped.
const URL_ESCAPED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn url_encode(value: &str) -> String {
    utf8_percent_encode(value, URL_ESCAPED).to_string()
}

#[derive(Debug, PartialEq, Eq)]
struct RealmInfo {
    federation_metadata_url: String,
    cloud_audience_urn: String,
}

/// Parses the `UserRealm` response. msodbcsql matches the fields with
/// `strstr`; parsing the JSON accepts the same documents and also tolerates
/// whitespace and escaped characters.
fn parse_realm(body: &str) -> TdsResult<RealmInfo> {
    let realm_error = || failure(format!("Error getting realm info\n{}", quote(body)));
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| realm_error())?;
    match json.get("account_type").and_then(|v| v.as_str()) {
        Some("Federated") => {}
        // msodbcsql lets a managed account fall through to a SAML grant with an
        // empty assertion, which Entra rejects. Failing here says why.
        Some("Managed") => {
            return Err(failure(
                "the account is not federated. ActiveDirectoryIntegrated requires an \
                 identity federated with Entra ID through ADFS."
                    .to_string(),
            ));
        }
        _ => return Err(failure(format!("{}\nUnknown account type.", quote(body)))),
    }
    let federation_metadata_url = json
        .get("federation_metadata_url")
        .and_then(|v| v.as_str())
        .ok_or_else(realm_error)?
        .to_string();
    let cloud_audience_urn = json
        .get("cloud_audience_urn")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_CLOUD_AUDIENCE)
        .to_string();
    Ok(RealmInfo {
        federation_metadata_url,
        cloud_audience_urn,
    })
}

/// Picks the WS-Trust endpoint for Negotiate authentication, preferring 1.3.
/// Returns the endpoint and whether it speaks WS-Trust 1.3.
fn select_endpoint(mex: &str) -> TdsResult<(String, bool)> {
    let [wst2005, wst13] = find_endpoints(mex);
    let (candidates, is_wst13) = if wst13.is_empty() {
        (wst2005, false)
    } else {
        (wst13, true)
    };
    if candidates.is_empty() {
        return Err(failure("Could not find any endpoints".to_string()));
    }
    // msodbcsql spreads load across equivalent endpoints at random.
    let pick = (uuid::Uuid::new_v4().as_u128() % candidates.len() as u128) as usize;
    let endpoint = candidates.into_iter().nth(pick).unwrap_or_default();
    if !endpoint.starts_with("https://") {
        return Err(not_https());
    }
    Ok((endpoint, is_wst13))
}

/// Finds the ports whose binding references a policy that offers
/// `<http:NegotiateAuthentication`. Index 1 holds WS-Trust 1.3 endpoints and
/// index 0 WS-Trust 2005. A line-for-line port of the scan in
/// `AzureADAuth.cpp:461-535`, including its tolerance for either quote style.
fn find_endpoints(mex: &str) -> [Vec<String>; 2] {
    let mut endpoints: [Vec<String>; 2] = Default::default();
    let mut cursor = 0;
    while let Some((id_start, id_end)) = quoted_value_from(mex, cursor, mex_tag::POLICY_ID) {
        let Some(node_end) = find_from(mex, id_end, mex_tag::POLICY_END) else {
            break;
        };
        if mex[id_start..node_end].contains(mex_tag::NEGOTIATE_ASSERTION) {
            collect_bindings(mex, &mex[id_start..id_end], &mut endpoints);
        }
        cursor = node_end;
    }
    endpoints
}

fn collect_bindings(mex: &str, policy_id: &str, endpoints: &mut [Vec<String>; 2]) {
    let mut cursor = 0;
    while let Some((name_start, name_end)) = quoted_value_from(mex, cursor, mex_tag::BINDING_NAME) {
        let Some(node_end) = find_from(mex, name_end, mex_tag::BINDING_END) else {
            break;
        };
        let node = &mex[name_end..node_end];
        let references_policy = quoted_value_from(node, 0, mex_tag::POLICY_REFERENCE)
            .is_some_and(|(s, e)| node[s..e].strip_prefix('#') == Some(policy_id));
        if references_policy {
            let wst13 = mex[name_start..node_end].contains(WST13_ISSUE);
            collect_ports(
                mex,
                &mex[name_start..name_end],
                &mut endpoints[usize::from(wst13)],
            );
        }
        cursor = name_end;
    }
}

fn collect_ports(mex: &str, binding_name: &str, endpoints: &mut Vec<String>) {
    let mut cursor = 0;
    loop {
        // msodbcsql tries `name=` first and falls back to a bare `binding=`.
        let (named, value_start, value_end) =
            if let Some((s, e)) = quoted_value_from(mex, cursor, mex_tag::PORT_NAME) {
                (true, s, e)
            } else if let Some((s, e)) = quoted_value_from(mex, cursor, mex_tag::PORT_BINDING) {
                (false, s, e)
            } else {
                break;
            };
        cursor = value_end;
        let Some(node_end) = find_from(mex, value_end, mex_tag::PORT_END) else {
            continue;
        };
        let binding = if named {
            quoted_value_from(
                &mex[..node_end],
                value_start,
                mex_tag::PORT_BINDING_ATTRIBUTE,
            )
        } else {
            Some((value_start, value_end))
        };
        let Some((binding_start, binding_end)) = binding else {
            continue;
        };
        let reference = &mex[binding_start..binding_end];
        let matches = reference
            .find(':')
            .is_some_and(|colon| &reference[colon + 1..] == binding_name);
        if !matches {
            continue;
        }
        let port = &mex[binding_end..node_end];
        if let (Some(open), Some(close)) =
            (port.find(mex_tag::ADDRESS), port.find(mex_tag::ADDRESS_END))
        {
            let start = open + mex_tag::ADDRESS.len();
            if start <= close {
                endpoints.push(port[start..close].to_string());
            }
        }
    }
}

/// Finds `key` at or after `from` and returns the byte range of the quoted
/// value that immediately follows it, in either quote style. Occurrences not
/// followed by a quote are skipped. msodbcsql's `ExtractQValue`.
fn quoted_value_from(buf: &str, from: usize, key: &str) -> Option<(usize, usize)> {
    let mut search = from;
    while let Some(found) = find_from(buf, search, key) {
        search = found + 1;
        let value = found + key.len();
        let quote = match buf.as_bytes().get(value) {
            Some(&q @ (b'"' | b'\'')) => q as char,
            _ => continue,
        };
        let start = value + 1;
        let end = buf.get(start..)?.find(quote)? + start;
        return Some((start, end));
    }
    None
}

fn find_from(buf: &str, from: usize, needle: &str) -> Option<usize> {
    buf.get(from..)?.find(needle).map(|i| i + from)
}

/// The WS-Trust `RequestSecurityToken` envelope, byte-for-byte as msodbcsql
/// builds it for Negotiate authentication (`AzureADAuth.cpp:549-603`): no
/// `wsse:Security` header, since the Kerberos exchange authenticates the call.
fn soap_request(endpoint: &str, wst13: bool, audience: &str, message_id: &str) -> String {
    let (action, namespace, key_type, request_type) = if wst13 {
        (
            WST13_ISSUE,
            WST13_NAMESPACE,
            WST13_BEARER_KEY,
            WST13_REQUEST_ISSUE,
        )
    } else {
        (
            WST2005_ISSUE,
            WST2005_NAMESPACE,
            WST2005_NO_PROOF_KEY,
            WST2005_REQUEST_ISSUE,
        )
    };
    format!(
        "<s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' \
         xmlns:wsa='http://www.w3.org/2005/08/addressing' \
         xmlns:wsu='http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd'>\
         <s:Header><wsa:Action s:mustUnderstand='1'>{action}</wsa:Action>\
         <wsa:messageID>urn:uuid:{message_id}</wsa:messageID>\
         <wsa:ReplyTo><wsa:Address>http://www.w3.org/2005/08/addressing/anonymous</wsa:Address></wsa:ReplyTo>\
         <wsa:To s:mustUnderstand='1'>{endpoint}</wsa:To></s:Header>\
         <s:Body><wst:RequestSecurityToken xmlns:wst='{namespace}'>\
         <wsp:AppliesTo xmlns:wsp='http://schemas.xmlsoap.org/ws/2004/09/policy'>\
         <wsa:EndpointReference><wsa:Address>{audience}</wsa:Address></wsa:EndpointReference></wsp:AppliesTo>\
         <wst:KeyType>{key_type}</wst:KeyType>\
         <wst:RequestType>{request_type}</wst:RequestType>\
         </wst:RequestSecurityToken></s:Body></s:Envelope>"
    )
}

/// The `<saml:Assertion>` (SAML 2.0) or `<saml1:Assertion>` (SAML 1.1)
/// element, tags included (`AzureADAuth.cpp:614-630`).
fn extract_assertion(response: &str) -> Option<&str> {
    for (open, close) in SAML_ASSERTION_TAGS {
        if let Some(start) = response.find(open)
            && let Some(end) = find_from(response, start, close)
        {
            return Some(&response[start..end + close.len()]);
        }
    }
    None
}

/// The SAML-bearer grant (`AzureADAuth.cpp:862-870`).
fn token_request_body(resource: &str, assertion: &str) -> String {
    let grant = if assertion.contains(SAML2_NAMESPACE) {
        "saml2-bearer"
    } else {
        "saml1_1-bearer"
    };
    format!(
        "resource={}&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3A{grant}\
         &assertion={}&client_id={CLIENT_ID}&scope=openid",
        url_encode(resource),
        url_encode(&BASE64.encode(assertion)),
    )
}

/// The STS cut before its fourth `/` — scheme, authority and tenant — plus the
/// token path (`AzureADAuth.cpp:890-897`).
fn token_url(sts_url: &str) -> String {
    let base = sts_url
        .match_indices('/')
        .nth(3)
        .map_or(sts_url, |(i, _)| &sts_url[..i]);
    format!("{base}/oauth2/token{TOKEN_API_VERSION}")
}

/// msodbcsql requires both `access_token` and `expires_on`
/// (`AzureADAuth.cpp:908-924`); Entra has sent `expires_on` as both a string
/// and a number.
fn parse_token_response(body: &str) -> TdsResult<String> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| {
        failure(format!(
            "the token response is not valid JSON\n{}",
            quote(body)
        ))
    })?;
    let token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| failure("the token response contains no access token".to_string()))?;
    if !json
        .get("expires_on")
        .is_some_and(|v| v.is_string() || v.is_number())
    {
        return Err(failure(
            "the token response contains no expiry time".to_string(),
        ));
    }
    Ok(token.to_string())
}

/// The most specific reason a failed token request offers: Entra's
/// `error_description`, else a SOAP fault reason, else the body
/// (`GetAccessTokenW`, `AzureADAuth.cpp:989-1000`).
fn token_error_description(body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(description) = json.get("error_description").and_then(|v| v.as_str())
    {
        return description.to_string();
    }
    let (reason, reason_end) = SOAP_FAULT_REASON;
    if let Some(start) = body.find(reason)
        && let Some(end) = find_from(body, start, reason_end)
    {
        return body[start + reason.len()..end].to_string();
    }
    quote(body).to_string()
}

/// Sends `request`, authenticating with Negotiate when `identity` is given,
/// under msodbcsql's retry policy.
async fn send_with_retry(
    transport: &dyn HttpTransport,
    identity: Option<&Arc<dyn KerberosIdentity>>,
    request: HttpRequest,
    policy: RetryPolicy,
) -> TdsResult<HttpResponse> {
    let started = Instant::now();
    let mut wait = policy.initial_wait;
    let mut retries = 0;
    loop {
        let response = match identity {
            Some(identity) => negotiate(transport, identity, &request).await?,
            None => transport.send(&request).await?,
        };
        if matches!(response.status, 200 | 401)
            || retries >= policy.max_retries
            || started.elapsed() >= policy.timeout
        {
            return Ok(response);
        }
        retries += 1;
        debug!(status = response.status, retries, url = %request.url, "integrated: retrying");
        tokio::time::sleep(wait).await;
        wait += wait;
    }
}

/// HTTP Negotiate (RFC 4559), as curl's `CURLAUTH_GSSNEGOTIATE` performs it:
/// the first request goes out unauthenticated and a `401 Negotiate` challenge
/// is answered with SPNEGO tokens until the server stops challenging.
async fn negotiate(
    transport: &dyn HttpTransport,
    identity: &Arc<dyn KerberosIdentity>,
    request: &HttpRequest,
) -> TdsResult<HttpResponse> {
    let mut response = transport.send(request).await?;
    let initial = match negotiate_challenge(&response) {
        Some(initial) if response.status == 401 => initial,
        _ => return Ok(response),
    };

    let host = url::Url::parse(&request.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .ok_or_else(|| failure(format!("invalid endpoint URL {}", request.url)))?;
    let mut context = blocking(Arc::clone(identity), move |id| id.negotiate_context(&host)).await?;

    // curl seeds the context from a token on the first challenge too.
    let mut challenge = initial;
    for _ in 0..MAX_NEGOTIATE_ROUNDS {
        let (returned, token) = run_blocking(move || {
            let token = context.generate_token(challenge.as_deref());
            (context, token)
        })
        .await?;
        context = returned;
        let token = token.map_err(Error::Security)?;

        let mut authenticated = request.clone();
        authenticated.headers.push((
            "Authorization".to_string(),
            format!("{NEGOTIATE} {}", BASE64.encode(&token.data)),
        ));
        response = transport.send(&authenticated).await?;
        if context.is_complete() {
            return Ok(response);
        }
        if response.status != 401 {
            // RFC 4559 §5: the final mutual-authentication token may ride on
            // the successful response. Verify it rather than drop it; a server
            // that omits it is still authenticated by TLS.
            if let Some(Some(last)) = negotiate_challenge(&response) {
                let (_, verified) = run_blocking(move || {
                    let verified = context.generate_token(Some(&last));
                    (context, verified)
                })
                .await?;
                verified.map_err(Error::Security)?;
            }
            return Ok(response);
        }
        match negotiate_challenge(&response) {
            Some(Some(next)) => challenge = Some(next),
            _ => return Ok(response),
        }
    }
    Ok(response)
}

/// `None` when the response offers no Negotiate challenge, `Some(None)` for a
/// bare `Negotiate`, and `Some(Some(token))` when it carries a server token.
fn negotiate_challenge(response: &HttpResponse) -> Option<Option<Vec<u8>>> {
    response
        .www_authenticate
        .iter()
        .flat_map(|header| header.split(','))
        .map(str::trim)
        .find_map(|challenge| {
            let (scheme, rest) = challenge.split_once(' ').unwrap_or((challenge, ""));
            scheme
                .eq_ignore_ascii_case(NEGOTIATE)
                .then(|| BASE64.decode(rest.trim()).ok().filter(|t| !t.is_empty()))
        })
}

async fn blocking<T, F>(identity: Arc<dyn KerberosIdentity>, call: F) -> TdsResult<T>
where
    T: Send + 'static,
    F: FnOnce(&dyn KerberosIdentity) -> TdsResult<T> + Send + 'static,
{
    run_blocking(move || call(identity.as_ref())).await?
}

/// GSSAPI may contact the KDC, so it runs off the async workers.
async fn run_blocking<T, F>(call: F) -> TdsResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(mssql_tds::trace_context::propagate(call))
        .await
        .map_err(|e| {
            Error::Security(SecurityError::InternalError(format!(
                "Kerberos processing did not run to completion: {e}"
            )))
        })
}

fn failure(detail: String) -> Error {
    Error::Security(SecurityError::AuthenticationDenied(format!(
        "{LABEL}: {detail}"
    )))
}

fn quote(body: &str) -> &str {
    if body.len() <= MAX_QUOTED_BODY {
        return body;
    }
    let mut end = MAX_QUOTED_BODY;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use mssql_tds::security::SspiAuthToken;
    use mssql_tds::security::mock::MockSecurityContext;
    use std::sync::Mutex;

    const STS: &str = "https://login.microsoftonline.com/contoso-tenant/";
    const RESOURCE: &str = "https://database.windows.net/";
    const MEX_URL: &str = "https://adfs.contoso.com/adfs/services/trust/mex";
    const WST13_URL: &str = "https://adfs.contoso.com/adfs/services/trust/13/windowstransport";
    const WST2005_URL: &str = "https://adfs.contoso.com/adfs/services/trust/2005/windowstransport";
    const SAML2: &str = "<saml:Assertion MajorVersion=\"1\" xmlns:saml=\"urn:oasis:names:tc:SAML:2.0:assertion\">alice</saml:Assertion>";

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            www_authenticate: Vec::new(),
            body: body.to_string(),
        }
    }

    fn challenge(header: &str) -> HttpResponse {
        HttpResponse {
            status: 401,
            www_authenticate: vec![header.to_string()],
            body: String::new(),
        }
    }

    fn federated_realm() -> String {
        format!(
            r#"{{"ver":"1.0","account_type":"Federated","domain_name":"contoso.com","federation_metadata_url":"{MEX_URL}","cloud_audience_urn":"urn:federation:contoso"}}"#
        )
    }

    fn negotiate_policy(id: &str) -> String {
        format!(
            r#"<wsp:Policy wsu:Id="{id}"><wsp:ExactlyOne><wsp:All><http:NegotiateAuthentication xmlns:http="http://schemas.microsoft.com/ws/06/2004/policy/http"/></wsp:All></wsp:ExactlyOne></wsp:Policy>"#
        )
    }

    fn username_policy(id: &str) -> String {
        format!(
            r#"<wsp:Policy wsu:Id="{id}"><wsp:ExactlyOne><wsp:All><sp:UsernameToken/></wsp:All></wsp:ExactlyOne></wsp:Policy>"#
        )
    }

    fn binding(name: &str, policy: &str, action: &str) -> String {
        format!(
            r##"<wsdl:binding name="{name}" type="tns:IWSTrust"><wsp:PolicyReference URI="#{policy}"/><soap12:binding transport="http://schemas.xmlsoap.org/soap/http"/><wsdl:operation name="Issue"><soap12:operation soapAction="{action}" style="document"/></wsdl:operation></wsdl:binding>"##
        )
    }

    fn port(binding: &str, address: &str) -> String {
        format!(
            r#"<wsdl:port name="{binding}_port" binding="tns:{binding}"><soap12:address location="{address}"/><wsa10:EndpointReference><wsa10:Address>{address}</wsa10:Address></wsa10:EndpointReference></wsdl:port>"#
        )
    }

    /// A trimmed ADFS MEX document offering Negotiate on WS-Trust 1.3 and 2005
    /// plus a username endpoint that must be ignored.
    fn mex() -> String {
        [
            negotiate_policy("WindowsTransport_policy"),
            username_policy("UserName13_policy"),
            binding("WindowsTransport", "WindowsTransport_policy", WST2005_ISSUE),
            binding("WindowsTransport13", "WindowsTransport_policy", WST13_ISSUE),
            binding("UserName13", "UserName13_policy", WST13_ISSUE),
            "<wsdl:service name=\"SecurityTokenService\">".to_string(),
            port("WindowsTransport", WST2005_URL),
            port("WindowsTransport13", WST13_URL),
            port(
                "UserName13",
                "https://adfs.contoso.com/adfs/services/trust/13/usernamemixed",
            ),
            "</wsdl:service>".to_string(),
        ]
        .concat()
    }

    fn saml_response(assertion: &str) -> String {
        format!(
            "<s:Envelope><s:Body><trust:RequestSecurityTokenResponseCollection><trust:RequestedSecurityToken>{assertion}</trust:RequestedSecurityToken></trust:RequestSecurityTokenResponseCollection></s:Body></s:Envelope>"
        )
    }

    const TOKEN_RESPONSE: &str =
        r#"{"token_type":"Bearer","expires_on":"1700000000","access_token":"eyJ.access.token"}"#;

    type Responder = dyn Fn(&HttpRequest, usize) -> TdsResult<HttpResponse> + Send + Sync;

    /// Answers each request from `respond`, which also receives how many
    /// earlier requests went to the same URL.
    struct ScriptedTransport {
        requests: Mutex<Vec<HttpRequest>>,
        respond: Box<Responder>,
    }

    impl ScriptedTransport {
        fn new(
            respond: impl Fn(&HttpRequest, usize) -> TdsResult<HttpResponse> + Send + Sync + 'static,
        ) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                respond: Box::new(respond),
            }
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().expect("lock").clone()
        }

        fn calls_to(&self, url_prefix: &str) -> usize {
            self.requests()
                .iter()
                .filter(|r| r.url.starts_with(url_prefix))
                .count()
        }
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn send(&self, request: &HttpRequest) -> TdsResult<HttpResponse> {
            let earlier = {
                let mut requests = self.requests.lock().expect("lock");
                let earlier = requests.iter().filter(|r| r.url == request.url).count();
                requests.push(request.clone());
                earlier
            };
            (self.respond)(request, earlier)
        }
    }

    /// A healthy federation: realm, MEX, an ADFS endpoint that challenges once
    /// and then accepts, and the token endpoint.
    fn happy(request: &HttpRequest, _earlier: usize) -> TdsResult<HttpResponse> {
        let url = request.url.as_str();
        Ok(if url.contains("/common/UserRealm/") {
            response(200, &federated_realm())
        } else if url == MEX_URL {
            response(200, &mex())
        } else if url.starts_with("https://adfs.contoso.com/") {
            if has_authorization(request) {
                response(200, &saml_response(SAML2))
            } else {
                challenge("Negotiate")
            }
        } else if url.contains("/oauth2/token") {
            response(200, TOKEN_RESPONSE)
        } else {
            response(404, "unexpected URL")
        })
    }

    fn has_authorization(request: &HttpRequest) -> bool {
        authorization(request).is_some()
    }

    fn authorization(request: &HttpRequest) -> Option<&str> {
        request
            .headers
            .iter()
            .find(|(name, _)| name == "Authorization")
            .map(|(_, value)| value.as_str())
    }

    fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    struct FakeIdentity {
        principal: Result<String, String>,
        context: MockSecurityContext,
        hosts: Mutex<Vec<String>>,
        challenges: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
    }

    impl FakeIdentity {
        fn new(principal: &str) -> Arc<Self> {
            Self::with_context(principal, MockSecurityContext::single_round(vec![1, 2, 3]))
        }

        fn with_context(principal: &str, context: MockSecurityContext) -> Arc<Self> {
            Arc::new(Self {
                principal: Ok(principal.to_string()),
                context,
                hosts: Mutex::new(Vec::new()),
                challenges: Arc::default(),
            })
        }

        fn challenges(&self) -> Vec<Option<Vec<u8>>> {
            self.challenges.lock().expect("lock").clone()
        }
    }

    /// Records the server token passed to each `generate_token` round.
    struct RecordingContext {
        inner: MockSecurityContext,
        challenges: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
    }

    impl SecurityContext for RecordingContext {
        fn package_name(&self) -> &str {
            self.inner.package_name()
        }

        fn generate_token(
            &mut self,
            server_token: Option<&[u8]>,
        ) -> Result<SspiAuthToken, SecurityError> {
            self.challenges
                .lock()
                .expect("lock")
                .push(server_token.map(<[u8]>::to_vec));
            self.inner.generate_token(server_token)
        }

        fn is_complete(&self) -> bool {
            self.inner.is_complete()
        }

        fn spn(&self) -> &str {
            self.inner.spn()
        }
    }

    impl KerberosIdentity for FakeIdentity {
        fn principal(&self) -> TdsResult<String> {
            self.principal.clone().map_err(|message| {
                Error::Security(SecurityError::AcquireCredentialsFailed {
                    code: 0xd0000,
                    message,
                })
            })
        }

        fn negotiate_context(&self, host: &str) -> TdsResult<Box<dyn SecurityContext>> {
            self.hosts.lock().expect("lock").push(host.to_string());
            Ok(Box::new(RecordingContext {
                inner: self.context.clone(),
                challenges: Arc::clone(&self.challenges),
            }))
        }
    }

    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            initial_wait: Duration::ZERO,
            ..RetryPolicy::default()
        }
    }

    fn run(
        transport: &ScriptedTransport,
        identity: Arc<FakeIdentity>,
        sts: &str,
    ) -> TdsResult<String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(acquire_token(
                transport,
                identity,
                sts,
                RESOURCE,
                fast_retry(),
            ))
    }

    fn message(result: TdsResult<String>) -> String {
        match result {
            Ok(token) => panic!("expected a failure, got token {token}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn acquires_a_token_through_adfs() {
        let transport = ScriptedTransport::new(happy);
        let identity = FakeIdentity::new("alice@CONTOSO.COM");

        let token = run(&transport, Arc::clone(&identity), STS).expect("token");
        assert_eq!(token, "eyJ.access.token");

        let requests = transport.requests();
        let urls: Vec<&str> = requests.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://login.microsoftonline.com/common/UserRealm/alice%40CONTOSO.COM?api-version=1.0",
                MEX_URL,
                WST13_URL,
                WST13_URL,
                "https://login.microsoftonline.com/contoso-tenant/oauth2/token?api-version=2015-06-01",
            ]
        );
        assert!(requests[..2].iter().all(|r| r.method == HttpMethod::Get));

        // curl probes without credentials and answers the 401 with SPNEGO.
        assert_eq!(authorization(&requests[2]), None);
        assert_eq!(
            authorization(&requests[3]),
            Some(format!("Negotiate {}", BASE64.encode([1, 2, 3])).as_str())
        );
        assert_eq!(*identity.hosts.lock().expect("lock"), ["adfs.contoso.com"]);

        let soap = &requests[3];
        assert_eq!(soap.method, HttpMethod::Post);
        assert_eq!(header(soap, "SOAPAction"), Some(WST13_ISSUE));
        assert_eq!(
            header(soap, "Content-Type"),
            Some("application/soap+xml; charset=utf-8")
        );
        let envelope = soap.body.as_deref().expect("SOAP body");
        assert!(envelope.contains("<wsa:Address>urn:federation:contoso</wsa:Address>"));
        assert!(envelope.contains(&format!(
            "<wsa:To s:mustUnderstand='1'>{WST13_URL}</wsa:To>"
        )));

        let grant = &requests[4];
        assert_eq!(
            header(grant, "Content-Type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(
            grant.body.as_deref(),
            Some(token_request_body(RESOURCE, SAML2).as_str())
        );
        assert!(!requests.iter().any(|r| r.url.starts_with("http://")));
    }

    #[test]
    fn a_malformed_sts_is_refused_before_any_request() {
        for (sts, expected) in [
            ("http://login.microsoftonline.com/tenant/", "must use https"),
            ("https://login.microsoftonline.com", "has no path"),
        ] {
            let transport = ScriptedTransport::new(happy);
            let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), sts));
            assert!(error.contains(expected), "{error}");
            assert!(transport.requests().is_empty());
        }
    }

    #[test]
    fn a_kerberos_failure_stops_the_flow() {
        let transport = ScriptedTransport::new(happy);
        let identity = Arc::new(FakeIdentity {
            principal: Err("Error acquiring Kerberos credentials".to_string()),
            context: MockSecurityContext::single_round(vec![1]),
            hosts: Mutex::new(Vec::new()),
            challenges: Arc::default(),
        });
        let error = message(run(&transport, identity, STS));
        assert!(
            error.contains("Error acquiring Kerberos credentials"),
            "{error}"
        );
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn a_managed_account_is_reported_as_not_federated() {
        let transport = ScriptedTransport::new(|request, _| {
            Ok(if request.url.contains("UserRealm") {
                response(
                    200,
                    r#"{"ver":"1.0","account_type":"Managed","domain_name":"contoso.onmicrosoft.com"}"#,
                )
            } else {
                response(500, "unexpected")
            })
        });
        let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS));
        assert!(error.contains("not federated"), "{error}");
        assert_eq!(transport.requests().len(), 1);
    }

    #[test]
    fn realm_failures_quote_the_response() {
        let transport =
            ScriptedTransport::new(|_, _| Ok(response(400, r#"{"error":"bad_request"}"#)));
        let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS));
        assert!(error.contains("Error getting realm info"), "{error}");
        assert!(error.contains("bad_request"), "{error}");
    }

    #[test]
    fn mex_failures_quote_the_response() {
        let transport = ScriptedTransport::new(|request, earlier| {
            if request.url == MEX_URL {
                Ok(response(404, "no mex here"))
            } else {
                happy(request, earlier)
            }
        });
        let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS));
        assert!(error.contains("Error getting MEX"), "{error}");
        assert!(error.contains("no mex here"), "{error}");
    }

    #[test]
    fn a_401_without_negotiate_is_not_answered() {
        let transport = ScriptedTransport::new(|request, earlier| {
            if request
                .url
                .starts_with("https://adfs.contoso.com/adfs/services/trust/13")
            {
                Ok(challenge("Basic realm=\"adfs\""))
            } else {
                happy(request, earlier)
            }
        });
        let identity = FakeIdentity::new("alice@CONTOSO.COM");
        let error = message(run(&transport, Arc::clone(&identity), STS));
        assert!(error.contains("Error requesting SAML token"), "{error}");
        assert_eq!(transport.calls_to(WST13_URL), 1, "401 is never retried");
        assert!(identity.hosts.lock().expect("lock").is_empty());
    }

    #[test]
    fn a_rejected_kerberos_ticket_fails_without_looping() {
        let transport = ScriptedTransport::new(|request, earlier| {
            if request.url == WST13_URL {
                Ok(challenge("Negotiate"))
            } else {
                happy(request, earlier)
            }
        });
        let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS));
        assert!(error.contains("Error requesting SAML token"), "{error}");
        assert_eq!(transport.calls_to(WST13_URL), 2);
    }

    #[test]
    fn a_multi_round_negotiate_forwards_the_server_token() {
        let server_token = vec![9, 8, 7];
        let context = MockSecurityContext::multi_round(vec![1], vec![2])
            .with_expected_challenges(vec![server_token.clone()]);
        let challenge_header = format!("Negotiate {}", BASE64.encode(&server_token));
        let transport = ScriptedTransport::new(move |request, earlier| {
            if request.url != WST13_URL {
                return happy(request, earlier);
            }
            Ok(match earlier {
                0 => challenge("Negotiate"),
                1 => challenge(&challenge_header),
                _ => response(200, &saml_response(SAML2)),
            })
        });
        let token = run(
            &transport,
            FakeIdentity::with_context("alice@CONTOSO.COM", context),
            STS,
        )
        .expect("token");
        assert_eq!(token, "eyJ.access.token");
        let rounds: Vec<Option<String>> = transport
            .requests()
            .iter()
            .filter(|r| r.url == WST13_URL)
            .map(|r| authorization(r).map(str::to_string))
            .collect();
        assert_eq!(
            rounds,
            [
                None,
                Some(format!("Negotiate {}", BASE64.encode([1]))),
                Some(format!("Negotiate {}", BASE64.encode([2]))),
            ]
        );
    }

    #[test]
    fn a_token_on_the_first_negotiate_challenge_seeds_the_context() {
        let server_token = vec![9, 8, 7];
        let challenge_header = format!("Negotiate {}", BASE64.encode(&server_token));
        let transport = ScriptedTransport::new(move |request, earlier| {
            if request.url != WST13_URL {
                return happy(request, earlier);
            }
            Ok(match earlier {
                0 => challenge(&challenge_header),
                _ => response(200, &saml_response(SAML2)),
            })
        });
        let identity = FakeIdentity::new("alice@CONTOSO.COM");
        let token = run(&transport, Arc::clone(&identity), STS).expect("token");
        assert_eq!(token, "eyJ.access.token");
        assert_eq!(identity.challenges(), [Some(server_token)]);
    }

    #[test]
    fn a_final_negotiate_token_on_success_is_verified() {
        let final_token = vec![9, 8, 7];
        let header = format!("Negotiate {}", BASE64.encode(&final_token));
        let script = |header: String| {
            ScriptedTransport::new(move |request, earlier| {
                if request.url != WST13_URL {
                    return happy(request, earlier);
                }
                Ok(match earlier {
                    0 => challenge("Negotiate"),
                    _ => HttpResponse {
                        www_authenticate: vec![header.clone()],
                        ..response(200, &saml_response(SAML2))
                    },
                })
            })
        };
        let context = || {
            MockSecurityContext::multi_round(vec![1], vec![2])
                .with_expected_challenges(vec![final_token.clone()])
        };

        let transport = script(header);
        let token = run(
            &transport,
            FakeIdentity::with_context("alice@CONTOSO.COM", context()),
            STS,
        )
        .expect("token");
        assert_eq!(token, "eyJ.access.token");
        let soap_sends = transport
            .requests()
            .iter()
            .filter(|r| r.url == WST13_URL)
            .count();
        assert_eq!(soap_sends, 2, "the accepted request must not be replayed");

        // A final token that fails verification fails mutual authentication.
        let forged = script(format!("Negotiate {}", BASE64.encode([1, 1, 1])));
        let error = run(
            &forged,
            FakeIdentity::with_context("alice@CONTOSO.COM", context()),
            STS,
        )
        .expect_err("forged final token");
        assert!(
            matches!(error, Error::Security(SecurityError::InvalidToken)),
            "{error:?}"
        );
        assert!(
            !forged
                .requests()
                .iter()
                .any(|r| r.url.contains("/oauth2/token")),
            "no token exchange after failed mutual authentication"
        );
    }

    #[test]
    fn a_security_context_error_is_surfaced() {
        let context = MockSecurityContext::single_round(vec![1]).with_error_on_round(
            0,
            SecurityError::InitContextFailed {
                code: 0xd0000,
                message: "Server not found in Kerberos database".to_string(),
            },
        );
        let transport = ScriptedTransport::new(happy);
        let error = message(run(
            &transport,
            FakeIdentity::with_context("alice@CONTOSO.COM", context),
            STS,
        ));
        assert!(
            error.contains("Server not found in Kerberos database"),
            "{error}"
        );
    }

    #[test]
    fn transient_statuses_are_retried() {
        let transport = ScriptedTransport::new(|request, earlier| {
            if request.url.contains("UserRealm") && earlier < 2 {
                Ok(response(503, "busy"))
            } else {
                happy(request, earlier)
            }
        });
        run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS).expect("token");
        assert_eq!(
            transport.calls_to("https://login.microsoftonline.com/common/"),
            3
        );
    }

    #[test]
    fn retries_stop_after_three() {
        let transport = ScriptedTransport::new(|_, _| Ok(response(503, "busy")));
        let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS));
        assert!(error.contains("Error getting realm info"), "{error}");
        assert_eq!(transport.requests().len(), 4);
    }

    #[test]
    fn transport_errors_are_not_retried() {
        let transport = ScriptedTransport::new(|_, _| {
            Err(Error::ConnectionError("connection refused".to_string()))
        });
        let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS));
        assert!(error.contains("connection refused"), "{error}");
        assert_eq!(transport.requests().len(), 1);
    }

    #[test]
    fn token_endpoint_errors_prefer_the_description() {
        let transport = ScriptedTransport::new(|request, earlier| {
            if request.url.contains("/oauth2/token") {
                Ok(response(
                    400,
                    r#"{"error":"invalid_grant","error_description":"AADSTS50008: SAML token is invalid."}"#,
                ))
            } else {
                happy(request, earlier)
            }
        });
        let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS));
        assert!(
            error.contains("AADSTS50008: SAML token is invalid."),
            "{error}"
        );
        assert!(!error.contains("invalid_grant"), "{error}");
    }

    #[test]
    fn a_response_without_an_assertion_fails() {
        let secret = "<saml2:Assertion>bearer-credential</saml2:Assertion>";
        let transport = ScriptedTransport::new(move |request, earlier| {
            if request.url == WST13_URL && has_authorization(request) {
                Ok(response(
                    200,
                    &format!("<s:Envelope><s:Body>{secret}</s:Body></s:Envelope>"),
                ))
            } else {
                happy(request, earlier)
            }
        });
        let error = message(run(&transport, FakeIdentity::new("alice@CONTOSO.COM"), STS));
        assert!(
            error.contains("SAML token not found in response"),
            "{error}"
        );
        assert!(
            !error.contains("bearer-credential"),
            "body must be withheld: {error}"
        );
    }

    #[test]
    fn url_encoding_matches_msodbcsql() {
        assert_eq!(url_encode("AZaz09-_.~"), "AZaz09-_.~");
        assert_eq!(url_encode("alice@contoso.com"), "alice%40contoso.com");
        assert_eq!(url_encode("a b/+=:"), "a%20b%2F%2B%3D%3A");
        assert_eq!(url_encode("é"), "%C3%A9");
    }

    #[test]
    fn realm_user_honours_an_alternate_domain() {
        assert_eq!(realm_user("alice@CONTOSO.COM"), "alice%40CONTOSO.COM");
        assert_eq!(
            realm_user("alice@contoso.com@CORP.CONTOSO.COM"),
            "alice%40contoso.com"
        );
        assert_eq!(
            realm_user("alice\\@contoso.com@CORP.CONTOSO.COM"),
            "alice%40contoso.com"
        );
        assert_eq!(realm_user("alice"), "alice");
    }

    #[test]
    fn sts_host_end_requires_https_and_a_path() {
        assert_eq!(sts_host_end(STS), Some(33));
        assert_eq!(&STS[..=33], "https://login.microsoftonline.com/");
        assert_eq!(sts_host_end("http://login.microsoftonline.com/t/"), None);
        assert_eq!(sts_host_end("https://login.microsoftonline.com"), None);
    }

    #[test]
    fn realm_parsing() {
        let info = parse_realm(&federated_realm()).expect("federated");
        assert_eq!(info.federation_metadata_url, MEX_URL);
        assert_eq!(info.cloud_audience_urn, "urn:federation:contoso");

        let info = parse_realm(&format!(
            r#"{{ "account_type" : "Federated", "federation_metadata_url" : "{MEX_URL}" }}"#
        ))
        .expect("whitespace and no audience");
        assert_eq!(info.cloud_audience_urn, DEFAULT_CLOUD_AUDIENCE);

        let unknown = parse_realm(r#"{"account_type":"Unknown"}"#).unwrap_err();
        assert!(
            unknown.to_string().contains("Unknown account type"),
            "{unknown}"
        );

        let no_mex = parse_realm(r#"{"account_type":"Federated"}"#).unwrap_err();
        assert!(
            no_mex.to_string().contains("Error getting realm info"),
            "{no_mex}"
        );

        let not_json = parse_realm("<html/>").unwrap_err();
        assert!(
            not_json.to_string().contains("Error getting realm info"),
            "{not_json}"
        );
    }

    #[test]
    fn mex_prefers_ws_trust_13() {
        assert_eq!(
            select_endpoint(&mex()).expect("endpoint"),
            (WST13_URL.to_string(), true)
        );
        let [wst2005, wst13] = find_endpoints(&mex());
        assert_eq!(wst2005, [WST2005_URL]);
        assert_eq!(wst13, [WST13_URL], "the username endpoint must be ignored");
    }

    #[test]
    fn mex_falls_back_to_ws_trust_2005() {
        let document = [
            negotiate_policy("WindowsTransport_policy"),
            binding("WindowsTransport", "WindowsTransport_policy", WST2005_ISSUE),
            port("WindowsTransport", WST2005_URL),
        ]
        .concat();
        assert_eq!(
            select_endpoint(&document).expect("endpoint"),
            (WST2005_URL.to_string(), false)
        );
    }

    #[test]
    fn mex_without_negotiate_has_no_endpoints() {
        let document = [
            username_policy("UserName13_policy"),
            binding("UserName13", "UserName13_policy", WST13_ISSUE),
            port(
                "UserName13",
                "https://adfs.contoso.com/adfs/services/trust/13/usernamemixed",
            ),
        ]
        .concat();
        let error = select_endpoint(&document).unwrap_err();
        assert!(
            error.to_string().contains("Could not find any endpoints"),
            "{error}"
        );
    }

    #[test]
    fn a_plain_http_endpoint_is_refused() {
        let document = [
            negotiate_policy("WindowsTransport_policy"),
            binding("WindowsTransport13", "WindowsTransport_policy", WST13_ISSUE),
            port(
                "WindowsTransport13",
                "http://adfs.contoso.com/adfs/services/trust/13/windowstransport",
            ),
        ]
        .concat();
        let error = select_endpoint(&document).unwrap_err();
        assert!(error.to_string().contains("must use https"), "{error}");
    }

    #[test]
    fn mex_accepts_single_quotes_and_binding_only_ports() {
        let document = [
            negotiate_policy("WindowsTransport_policy").replace('"', "'"),
            binding("WindowsTransport13", "WindowsTransport_policy", WST13_ISSUE).replace('"', "'"),
            format!(
                "<wsdl:port binding='tns:WindowsTransport13'><wsa10:EndpointReference><wsa10:Address>{WST13_URL}</wsa10:Address></wsa10:EndpointReference></wsdl:port>"
            ),
        ]
        .concat();
        assert_eq!(
            select_endpoint(&document).expect("endpoint"),
            (WST13_URL.to_string(), true)
        );
    }

    #[test]
    fn soap_envelopes_match_msodbcsql() {
        let wst13 = soap_request(WST13_URL, true, "urn:federation:MicrosoftOnline", "0-1");
        assert!(wst13.starts_with(
            "<s:Envelope xmlns:s='http://www.w3.org/2003/05/soap-envelope' xmlns:wsa="
        ));
        assert!(wst13.ends_with("</wst:RequestSecurityToken></s:Body></s:Envelope>"));
        assert!(wst13.contains(&format!(
            "<wsa:Action s:mustUnderstand='1'>{WST13_ISSUE}</wsa:Action>"
        )));
        assert!(wst13.contains("<wsa:messageID>urn:uuid:0-1</wsa:messageID>"));
        assert!(wst13.contains("xmlns:wst='http://docs.oasis-open.org/ws-sx/ws-trust/200512'"));
        assert!(wst13.contains(
            "<wst:KeyType>http://docs.oasis-open.org/ws-sx/ws-trust/200512/Bearer</wst:KeyType>"
        ));
        assert!(wst13.contains("<wst:RequestType>http://docs.oasis-open.org/ws-sx/ws-trust/200512/Issue</wst:RequestType>"));
        assert!(
            !wst13.contains("wsse:Security"),
            "Kerberos authenticates the call"
        );
        assert!(!wst13.contains(">\n") && !wst13.contains("> <"));

        let wst2005 = soap_request(WST2005_URL, false, "urn:x", "0-1");
        assert!(wst2005.contains(&format!(
            "<wsa:Action s:mustUnderstand='1'>{WST2005_ISSUE}</wsa:Action>"
        )));
        assert!(wst2005.contains("xmlns:wst='http://schemas.xmlsoap.org/ws/2005/02/trust'"));
        assert!(wst2005.contains(
            "<wst:KeyType>http://schemas.xmlsoap.org/ws/2005/05/identity/NoProofKey</wst:KeyType>"
        ));
        assert!(wst2005.contains(
            "<wst:RequestType>http://schemas.xmlsoap.org/ws/2005/02/trust/Issue</wst:RequestType>"
        ));
    }

    #[test]
    fn assertion_extraction() {
        assert_eq!(extract_assertion(&saml_response(SAML2)), Some(SAML2));
        let saml1 = "<saml1:Assertion x='1'>bob</saml1:Assertion>";
        assert_eq!(extract_assertion(&saml_response(saml1)), Some(saml1));
        assert_eq!(extract_assertion("<saml:Assertion>unterminated"), None);
        assert_eq!(extract_assertion("<s:Envelope/>"), None);
    }

    #[test]
    fn grant_type_follows_the_saml_version() {
        let saml2 = token_request_body(RESOURCE, SAML2);
        assert!(saml2.starts_with("resource=https%3A%2F%2Fdatabase.windows.net%2F&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Asaml2-bearer&assertion="));
        assert!(saml2.ends_with("&client_id=2C1229AA-16C5-4FF5-B46B-4F7FE2A2A9C8&scope=openid"));
        assert!(saml2.contains(&format!(
            "&assertion={}&",
            url_encode(&BASE64.encode(SAML2))
        )));

        let saml1 = token_request_body(RESOURCE, "<saml1:Assertion>bob</saml1:Assertion>");
        assert!(saml1.contains("grant-type%3Asaml1_1-bearer&"));
    }

    #[test]
    fn token_url_keeps_the_tenant() {
        let expected =
            "https://login.microsoftonline.com/tenant/oauth2/token?api-version=2015-06-01";
        assert_eq!(
            token_url("https://login.microsoftonline.com/tenant/"),
            expected
        );
        assert_eq!(
            token_url("https://login.microsoftonline.com/tenant"),
            expected
        );
        assert_eq!(
            token_url("https://login.microsoftonline.com/tenant/v2.0/"),
            expected
        );
    }

    #[test]
    fn token_response_parsing() {
        assert_eq!(
            parse_token_response(TOKEN_RESPONSE).expect("token"),
            "eyJ.access.token"
        );
        assert_eq!(
            parse_token_response(r#"{"access_token":"t","expires_on":1700000000}"#)
                .expect("numeric expiry"),
            "t"
        );
        let no_token = parse_token_response(r#"{"expires_on":"1"}"#).unwrap_err();
        assert!(
            no_token.to_string().contains("no access token"),
            "{no_token}"
        );
        let no_expiry = parse_token_response(r#"{"access_token":"t"}"#).unwrap_err();
        assert!(no_expiry.to_string().contains("no expiry"), "{no_expiry}");
    }

    #[test]
    fn token_error_descriptions() {
        assert_eq!(
            token_error_description(r#"{"error":"x","error_description":"why"}"#),
            "why"
        );
        assert_eq!(
            token_error_description(
                "<s:Fault><s:Reason><s:Text>MSIS3127</s:Text></s:Reason></s:Fault>"
            ),
            "<s:Text>MSIS3127</s:Text>"
        );
        assert_eq!(token_error_description("plain"), "plain");
    }

    #[test]
    fn negotiate_challenge_parsing() {
        let parse = |headers: &[&str]| {
            negotiate_challenge(&HttpResponse {
                status: 401,
                www_authenticate: headers.iter().map(|h| h.to_string()).collect(),
                body: String::new(),
            })
        };
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["Basic realm=\"x\""]), None);
        assert_eq!(parse(&["Negotiate"]), Some(None));
        assert_eq!(parse(&["NTLM", "negotiate"]), Some(None));
        assert_eq!(parse(&["Negotiate, NTLM"]), Some(None));
        assert_eq!(parse(&["Negotiate AQID"]), Some(Some(vec![1, 2, 3])));
    }

    #[test]
    fn quoted_values_accept_either_quote_and_skip_bare_matches() {
        let buf = r#"<a id=x/><a id="one"/><a id='two'/>"#;
        let (s, e) = quoted_value_from(buf, 0, "<a id=").expect("first quoted");
        assert_eq!(&buf[s..e], "one");
        let (s, e) = quoted_value_from(buf, e, "<a id=").expect("second quoted");
        assert_eq!(&buf[s..e], "two");
        assert_eq!(quoted_value_from(buf, e, "<a id="), None);
        assert_eq!(quoted_value_from("<a id=\"open", 0, "<a id="), None);
    }

    #[test]
    fn long_bodies_are_truncated_on_a_char_boundary() {
        let body = "é".repeat(MAX_QUOTED_BODY);
        let quoted = quote(&body);
        assert!(quoted.len() <= MAX_QUOTED_BODY);
        assert!(body.starts_with(quoted));
    }
}

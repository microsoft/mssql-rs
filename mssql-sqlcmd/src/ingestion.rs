// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sending the one usage event to Application Insights.
//!
//! The event is a single `Microsoft.ApplicationInsights.Event` item, POSTed as a
//! one-element JSON array to `{ingestion endpoint}/v2/track`, which is the
//! public ingestion API. There is one attempt and no retry: the process waits
//! for the answer for at most [`crate::telemetry::MAX_FLUSH_TIMEOUT`], so a
//! retry could never complete in time.

use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use serde_json::Value as JsonValue;
use tracing::debug;

use crate::device_id;

const EVENT_ENVELOPE_NAME: &str = "Microsoft.ApplicationInsights.Event";
/// The global ingestion endpoint, used when a connection string names none.
const DEFAULT_INGESTION_ENDPOINT: &str = "https://dc.services.visualstudio.com";
/// A sanity bound on the endpoint; real ones are a few dozen characters.
const MAX_ENDPOINT_LEN: usize = 2048;
/// The most of a reply that is read: a 206 body is a few dozen bytes.
const MAX_REPLY_BYTES: u64 = 64 * 1024;

/// Where events go: the instrumentation key and the ingestion URL, both taken
/// from a connection string that has been checked.
#[derive(Debug)]
pub(crate) struct Destination {
    instrumentation_key: String,
    track_url: Url,
}

impl Destination {
    /// The destination a connection string names, or `None` unless it is one
    /// that can deliver: every part is `name=value`, the instrumentation key is a
    /// GUID, and the ingestion endpoint is an HTTPS URL with a host.
    ///
    /// Names are case-insensitive (ASCII), values are not trimmed, and when a name
    /// repeats the last one wins. A connection string without an endpoint uses the
    /// global one; `EndpointSuffix` (and `Location`) name a sovereign cloud's.
    /// Anything else in the string is ignored.
    pub(crate) fn parse(connection_string: &str) -> Option<Self> {
        let mut fields: HashMap<String, &str> = HashMap::new();
        for part in connection_string.split(';').filter(|part| !part.is_empty()) {
            let (name, value) = part.split_once('=')?;
            fields.insert(name.to_ascii_lowercase(), value);
        }

        let key = *fields.get("instrumentationkey")?;
        if !device_id::is_hyphenated_guid(key) {
            return None;
        }

        let endpoint = if let Some(endpoint) = fields.get("ingestionendpoint") {
            (*endpoint).to_owned()
        } else if let Some(suffix) = fields.get("endpointsuffix") {
            let location = fields
                .get("location")
                .map(|location| format!("{location}."))
                .unwrap_or_default();
            format!("https://{location}dc.{suffix}")
        } else {
            DEFAULT_INGESTION_ENDPOINT.to_owned()
        };

        Some(Self {
            instrumentation_key: key.to_owned(),
            track_url: track_url(&endpoint)?,
        })
    }

    /// The one-item request body for `event`, with `properties` as its custom
    /// properties. No tags are sent, so nothing identifies the machine or the
    /// user beyond the properties themselves.
    pub(crate) fn event_body(
        &self,
        event: &str,
        properties: &[(&str, String)],
        time: SystemTime,
    ) -> Option<Vec<u8>> {
        let properties: serde_json::Map<String, JsonValue> = properties
            .iter()
            .map(|(key, value)| ((*key).to_owned(), JsonValue::from(value.as_str())))
            .collect();
        serde_json::to_vec(&serde_json::json!([{
            "name": EVENT_ENVELOPE_NAME,
            "time": rfc3339_millis(time),
            "iKey": self.instrumentation_key,
            "data": {
                "baseType": "EventData",
                "baseData": { "ver": 2, "name": event, "properties": properties }
            }
        }]))
        .ok()
    }

    pub(crate) fn track_url(&self) -> &Url {
        &self.track_url
    }
}

/// `endpoint` with `/v2/track` appended, if it is an HTTPS URL with a host and
/// nothing else a request URL should not carry. Checked on the text first
/// because URL parsing is lenient: `https:///path` would parse with `path` as
/// the host.
fn track_url(endpoint: &str) -> Option<Url> {
    if endpoint.len() > MAX_ENDPOINT_LEN {
        return None;
    }
    let authority = endpoint.strip_prefix("https://")?;
    if authority.is_empty() || authority.starts_with(['/', '?', '#']) {
        return None;
    }
    let url = Url::parse(&format!("{}/v2/track", endpoint.trim_end_matches('/'))).ok()?;
    let usable = url.scheme() == "https"
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    usable.then_some(url)
}

/// An answer from the service.
#[derive(Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Sends a request body to the ingestion URL. A trait so that tests can answer
/// without a network.
pub(crate) trait Post: Send + Sync {
    fn post(&self, url: &Url, body: Vec<u8>) -> Result<Reply, String>;
}

/// The production transport.
pub(crate) struct HttpPost(reqwest::blocking::Client);

impl HttpPost {
    /// A client that gives up on connecting, or on the whole request, after
    /// `timeout`, and follows no redirect: the event goes to the configured
    /// endpoint and nowhere else.
    pub(crate) fn new(timeout: Duration) -> Result<Self, reqwest::Error> {
        reqwest::blocking::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(Self)
    }
}

impl Post for HttpPost {
    fn post(&self, url: &Url, body: Vec<u8>) -> Result<Reply, String> {
        let response = self
            .0
            .post(url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|error| error.without_url().to_string())?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        response
            .take(MAX_REPLY_BYTES)
            .read_to_end(&mut body)
            .map_err(|error| error.to_string())?;
        Ok(Reply { status, body })
    }
}

/// Posts `body` once and says whether the service took the event.
pub(crate) fn deliver(post: &dyn Post, destination: &Destination, body: Vec<u8>) -> bool {
    match post.post(destination.track_url(), body) {
        Ok(reply) => accepted(&reply),
        Err(error) => {
            debug!("sqlcmd telemetry send failed: {error}");
            false
        }
    }
}

/// Whether `reply` says the one event was taken: a 200, or a 206 that reports
/// one item received and one accepted. Every other answer, including any other
/// 2xx, a throttle or an error, is a failure.
fn accepted(reply: &Reply) -> bool {
    match reply.status {
        200 => true,
        206 => {
            let Ok(payload) = serde_json::from_slice::<JsonValue>(&reply.body) else {
                return false;
            };
            let count = |name: &str| payload.get(name).and_then(JsonValue::as_u64);
            count("itemsReceived") == Some(1) && count("itemsAccepted") == Some(1)
        }
        _ => false,
    }
}

/// `time` as UTC ISO 8601 with milliseconds and a trailing `Z`
/// (`2024-11-04T16:00:00.000Z`), which the service requires. A time before the
/// epoch is written as the epoch.
fn rfc3339_millis(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let in_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600,
        in_day % 3_600 / 60,
        in_day % 60,
        since_epoch.subsec_millis()
    )
}

/// The proleptic Gregorian date of a day count from 1970-01-01.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Mutex;

    const KEY: &str = "00000000-0000-0000-0000-000000000000";

    fn destination(connection_string: &str) -> Destination {
        Destination::parse(connection_string).expect("a valid connection string")
    }

    #[test]
    fn the_endpoint_and_key_come_from_the_connection_string() {
        let destination = destination(&format!(
            "InstrumentationKey={KEY};IngestionEndpoint=https://example.test/"
        ));
        assert_eq!(
            destination.track_url().as_str(),
            "https://example.test/v2/track"
        );
        assert_eq!(destination.instrumentation_key, KEY);
    }

    #[test]
    fn the_endpoint_forms_that_are_valid() {
        for (connection_string, expected) in [
            (
                format!("InstrumentationKey={KEY}"),
                "https://dc.services.visualstudio.com/v2/track",
            ),
            (
                format!("InstrumentationKey={KEY};IngestionEndpoint=https://example.test"),
                "https://example.test/v2/track",
            ),
            (
                format!("InstrumentationKey={KEY};IngestionEndpoint=https://example.test//"),
                "https://example.test/v2/track",
            ),
            (
                format!(
                    "InstrumentationKey={KEY};IngestionEndpoint=https://example.test:8443/prefix"
                ),
                "https://example.test:8443/prefix/v2/track",
            ),
            (
                format!("InstrumentationKey={KEY};EndpointSuffix=applicationinsights.azure.cn"),
                "https://dc.applicationinsights.azure.cn/v2/track",
            ),
            (
                format!(
                    "InstrumentationKey={KEY};EndpointSuffix=applicationinsights.azure.cn;Location=chinaeast"
                ),
                "https://chinaeast.dc.applicationinsights.azure.cn/v2/track",
            ),
            // Names fold in ASCII, an unknown field is ignored, and a trailing
            // separator is fine.
            (
                format!("INSTRUMENTATIONKEY={KEY};ingestionendpoint=https://example.test;Other=x;"),
                "https://example.test/v2/track",
            ),
            // The last of a repeated name wins.
            (
                format!(
                    "InstrumentationKey={KEY};IngestionEndpoint=https://one.test;IngestionEndpoint=https://two.test"
                ),
                "https://two.test/v2/track",
            ),
        ] {
            let destination = destination(&connection_string);
            assert_eq!(
                destination.track_url().as_str(),
                expected,
                "{connection_string}"
            );
        }
    }

    #[test]
    fn connection_strings_that_cannot_deliver_are_invalid() {
        let long_endpoint = format!("https://{}", "a".repeat(MAX_ENDPOINT_LEN));
        for invalid in [
            String::new(),
            "   ".to_owned(),
            "not a connection string".to_owned(),
            "InstrumentationKey=DO-NOT-LOG-THIS;broken".to_owned(),
            "InstrumentationKey=broken".to_owned(),
            "InstrumentationKey=00000000-0000-0000-0000-00000000000".to_owned(),
            "InstrumentationKey=00000000-0000-0000-0000-00000000000g".to_owned(),
            format!("InstrumentationKey= {KEY}"),
            format!("InstrumentationKey={KEY} "),
            // The last key is the one used.
            format!("InstrumentationKey={KEY};InstrumentationKey=broken"),
            "IngestionEndpoint=https://example.test".to_owned(),
            // No instrumentation key, or one that is not a GUID, can never deliver.
            format!("IngestionEndpoint=https://example.test;Other={KEY}"),
            // An endpoint that cannot be posted to.
            format!("InstrumentationKey={KEY};IngestionEndpoint=localhost"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=example.test"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=ftp://example.test"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=http://example.test"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=HTTPS://example.test"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=https://"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=https:///path"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=https://?query"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=https://example.test?query=1"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=https://example.test#fragment"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=https://user:secret@example.test"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=https://exa mple.test"),
            format!("InstrumentationKey={KEY};IngestionEndpoint=https://example.test "),
            format!(
                "InstrumentationKey={KEY};IngestionEndpoint=https://example.test;IngestionEndpoint=localhost"
            ),
            format!("InstrumentationKey={KEY};IngestionEndpoint={long_endpoint}"),
        ] {
            assert!(Destination::parse(&invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn the_body_is_one_event_with_no_tags() {
        let destination = destination(&format!(
            "InstrumentationKey={KEY};IngestionEndpoint=https://example.test"
        ));
        let body = destination
            .event_body(
                "sqlcmdcli_usage",
                &[
                    ("sqlcmdcli_version", "18.5".to_owned()),
                    ("sqlcmdcli_os", "windows".to_owned()),
                ],
                UNIX_EPOCH + Duration::from_secs(1_730_736_000),
            )
            .expect("a body");
        let items: Vec<JsonValue> = serde_json::from_slice(&body).expect("a JSON array");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0],
            serde_json::json!({
                "name": "Microsoft.ApplicationInsights.Event",
                "time": "2024-11-04T16:00:00.000Z",
                "iKey": KEY,
                "data": {
                    "baseType": "EventData",
                    "baseData": {
                        "ver": 2,
                        "name": "sqlcmdcli_usage",
                        "properties": {
                            "sqlcmdcli_version": "18.5",
                            "sqlcmdcli_os": "windows"
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn times_are_utc_iso_8601_with_milliseconds() {
        let at = |seconds: u64, millis: u32| {
            rfc3339_millis(UNIX_EPOCH + Duration::new(seconds, millis * 1_000_000))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        assert_eq!(at(1_730_736_000, 7), "2024-11-04T16:00:00.007Z");
        assert_eq!(at(1_709_251_199, 999), "2024-02-29T23:59:59.999Z");
        assert_eq!(at(4_102_444_800, 0), "2100-01-01T00:00:00.000Z");
        // Before the epoch is the epoch.
        assert_eq!(
            rfc3339_millis(UNIX_EPOCH - Duration::from_secs(5)),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn only_a_took_it_answer_is_delivery() {
        let reply = |status: u16, body: &str| Reply {
            status,
            body: body.as_bytes().to_vec(),
        };
        for (status, body, expected) in [
            (200, "", true),
            (
                200,
                r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#,
                true,
            ),
            (
                206,
                r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#,
                true,
            ),
            (206, r#"{"itemsReceived":1,"itemsAccepted":1}"#, true),
            (
                206,
                r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":0,"statusCode":429}]}"#,
                false,
            ),
            (
                206,
                r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[]}"#,
                false,
            ),
            (
                206,
                r#"{"itemsReceived":2,"itemsAccepted":1,"errors":[]}"#,
                false,
            ),
            (206, r#"{"itemsReceived":1,"itemsAccepted":"1"}"#, false),
            (206, r#"{"itemsReceived":1}"#, false),
            (206, "not json", false),
            (206, "[]", false),
            (206, "", false),
            // Another 2xx is not an acceptance, and neither is anything else.
            (201, "", false),
            (202, "", false),
            (204, "", false),
            (301, "", false),
            (400, "", false),
            (401, "", false),
            (403, "", false),
            (408, "", false),
            (429, "", false),
            (
                500,
                r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#,
                false,
            ),
            (503, "", false),
        ] {
            assert_eq!(accepted(&reply(status, body)), expected, "{status} {body}");
        }
    }

    /// Answers with what it is told and records what it was asked.
    struct Scripted {
        reply: Result<(u16, &'static str), &'static str>,
        calls: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl Scripted {
        fn new(reply: Result<(u16, &'static str), &'static str>) -> Self {
            Self {
                reply,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Post for Scripted {
        fn post(&self, url: &Url, body: Vec<u8>) -> Result<Reply, String> {
            self.calls
                .lock()
                .expect("calls lock")
                .push((url.to_string(), body));
            match self.reply {
                Ok((status, body)) => Ok(Reply {
                    status,
                    body: body.as_bytes().to_vec(),
                }),
                Err(error) => Err(error.to_owned()),
            }
        }
    }

    #[test]
    fn delivery_is_one_attempt_to_the_track_url() {
        let destination = destination(&format!(
            "InstrumentationKey={KEY};IngestionEndpoint=https://example.test"
        ));
        for (reply, expected) in [
            (Ok((200, "")), true),
            (Ok((206, r#"{"itemsReceived":1,"itemsAccepted":1}"#)), true),
            (Ok((503, "")), false),
            (Ok((429, "")), false),
            (Err("connection refused"), false),
        ] {
            let post = Scripted::new(reply);
            assert_eq!(
                deliver(&post, &destination, b"[]".to_vec()),
                expected,
                "{reply:?}"
            );
            let calls = post.calls.lock().expect("calls lock");
            assert_eq!(calls.len(), 1, "no retry: {reply:?}");
            assert_eq!(calls[0].0, "https://example.test/v2/track");
            assert_eq!(calls[0].1, b"[]");
        }
    }

    /// One request-response exchange on a local port, without TLS: the answer to
    /// give, and the request as the server read it.
    fn serve_once(answer: &'static str) -> (Url, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = Url::parse(&format!(
            "http://{}/v2/track",
            listener.local_addr().expect("addr")
        ))
        .expect("url");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            // Read until the headers and the whole body have arrived.
            loop {
                let read = stream.read(&mut chunk).expect("read");
                request.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            stream.write_all(answer.as_bytes()).expect("write");
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, server)
    }

    #[test]
    fn the_http_transport_posts_json_and_reads_the_answer() {
        let (url, server) = serve_once(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 37\r\nConnection: close\r\n\r\n{\"itemsReceived\":1,\"itemsAccepted\":1}",
        );
        let reply = HttpPost::new(Duration::from_secs(5))
            .expect("client")
            .post(&url, b"[1]".to_vec())
            .expect("a reply");
        let request = server.join().expect("server");

        assert_eq!(reply.status, 206);
        assert!(
            accepted(&reply),
            "{:?}",
            String::from_utf8_lossy(&reply.body)
        );
        assert!(request.starts_with("POST /v2/track HTTP/1.1"), "{request}");
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains("content-type: application/json"),
            "{request}"
        );
        assert!(request.ends_with("[1]"), "{request}");
    }

    #[test]
    fn the_http_transport_does_not_follow_a_redirect() {
        let (url, server) = serve_once(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let reply = HttpPost::new(Duration::from_secs(5))
            .expect("client")
            .post(&url, b"[1]".to_vec())
            .expect("a reply");
        server.join().expect("server");

        assert_eq!(reply.status, 302);
        assert!(!accepted(&reply));
    }

    #[test]
    fn the_http_transport_reports_a_refused_connection_as_an_error() {
        // A port nothing listens on: bind to learn a free one, then close it.
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/v2/track")).expect("url");
        let error = HttpPost::new(Duration::from_secs(5))
            .expect("client")
            .post(&url, b"[1]".to_vec())
            .expect_err("nothing is listening");
        // The URL is not part of the message.
        assert!(!error.contains("127.0.0.1"), "{error}");
    }
}

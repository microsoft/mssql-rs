// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Privacy-safe Application Insights usage telemetry for native sqlcmd.

use std::fmt;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use http::{Request, Response};
use opentelemetry::trace::{Span, Tracer, TracerProvider as _};
use opentelemetry::{KeyValue, Value};
use opentelemetry_application_insights::{Exporter, HttpClient};
use opentelemetry_http::HttpError;
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider};
use serde_json::Value as JsonValue;
use tracing::debug;

pub const USAGE_EVENT_NAME: &str = "sqlcmdcli_usage";
pub const CONNECTION_STRING_ENV: &str = "APPLICATIONINSIGHTS_CONNECTION_STRING";

const CUSTOM_EVENT_OTEL_NAME: &str = "ai.custom";
const CUSTOM_EVENT_NAME_ATTRIBUTE: &str = "ai.customEvent.name";
const APPLICATION_INSIGHTS_EVENT_ENVELOPE: &str = "Microsoft.ApplicationInsights.Event";
const SCHEMA_VERSION: &str = "1";
const IMPLEMENTATION: &str = "mssql-sqlcmd-rust";
const RUNTIME: &str = "native";
const MAX_VERSION_LEN: usize = 64;
pub const MAX_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

static PROVIDER: OnceLock<Mutex<Option<SdkTracerProvider>>> = OnceLock::new();
static SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushOutcome {
    Completed,
    Failed,
    TimedOut,
}

#[derive(Debug)]
struct EventOnlyClient<C> {
    inner: C,
}

impl<C> EventOnlyClient<C> {
    fn new(inner: C) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl<C> HttpClient for EventOnlyClient<C>
where
    C: HttpClient,
{
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let Some(filtered) = filter_event_request(request)? else {
            return Response::builder()
                .status(200)
                .body(Bytes::new())
                .map_err(|error| Box::new(error) as HttpError);
        };
        let FilteredRequest {
            request,
            original_indices,
            original_count,
        } = filtered;
        let response = self.inner.send_bytes(request).await?;
        translate_partial_response(response, &original_indices, original_count)
    }
}

#[derive(Debug)]
struct FilteredRequest {
    request: Request<Bytes>,
    original_indices: Vec<usize>,
    original_count: usize,
}

#[derive(Debug)]
struct FilterError(String);

impl fmt::Display for FilterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FilterError {}

fn filter_event_request(request: Request<Bytes>) -> Result<Option<FilteredRequest>, HttpError> {
    let (parts, body) = request.into_parts();
    let mut decompressed = Vec::new();
    GzDecoder::new(body.as_ref())
        .read_to_end(&mut decompressed)
        .map_err(filter_error)?;

    let envelopes: Vec<JsonValue> = serde_json::from_slice(&decompressed).map_err(filter_error)?;
    let original_count = envelopes.len();
    let (original_indices, envelopes): (Vec<_>, Vec<_>) = envelopes
        .into_iter()
        .enumerate()
        .filter(|(_, envelope)| {
            envelope.get("name").and_then(JsonValue::as_str)
                == Some(APPLICATION_INSIGHTS_EVENT_ENVELOPE)
        })
        .unzip();
    if original_indices.is_empty() {
        return Ok(None);
    }

    let serialized = serde_json::to_vec(&envelopes).map_err(filter_error)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&serialized).map_err(filter_error)?;
    let compressed = encoder.finish().map_err(filter_error)?;
    Ok(Some(FilteredRequest {
        request: Request::from_parts(parts, Bytes::from(compressed)),
        original_indices,
        original_count,
    }))
}

fn translate_partial_response(
    response: Response<Bytes>,
    original_indices: &[usize],
    original_count: usize,
) -> Result<Response<Bytes>, HttpError> {
    if !matches!(response.status().as_u16(), 206 | 500) {
        return Ok(response);
    }

    let (parts, body) = response.into_parts();
    let mut payload: JsonValue = serde_json::from_slice(&body).map_err(filter_error)?;
    let dropped_count = original_count.saturating_sub(original_indices.len());
    payload["itemsReceived"] = JsonValue::from(original_count);
    if let Some(items_accepted) = payload.get("itemsAccepted").and_then(JsonValue::as_u64) {
        payload["itemsAccepted"] = JsonValue::from(items_accepted + dropped_count as u64);
    }
    if let Some(errors) = payload.get_mut("errors").and_then(JsonValue::as_array_mut) {
        for error in errors {
            let Some(filtered_index) = error.get("index").and_then(JsonValue::as_u64) else {
                continue;
            };
            let Some(original_index) = original_indices.get(filtered_index as usize) else {
                return Err(filter_error("invalid Application Insights response index"));
            };
            error["index"] = JsonValue::from(*original_index);
        }
    }
    let body = serde_json::to_vec(&payload).map_err(filter_error)?;
    Ok(Response::from_parts(parts, Bytes::from(body)))
}

fn filter_error(error: impl fmt::Display) -> HttpError {
    Box::new(FilterError(error.to_string()))
}

/// Records the single usage event for one native sqlcmd run.
///
/// The exporter is initialized lazily from
/// [`APPLICATIONINSIGHTS_CONNECTION_STRING`](CONNECTION_STRING_ENV). Missing or
/// invalid configuration is a no-op. Telemetry failures never affect sqlcmd.
pub fn record_usage(sqlcmd_version: Option<&str>) {
    if SHUTDOWN_STARTED.load(Ordering::Acquire) {
        return;
    }
    let provider = PROVIDER.get_or_init(|| Mutex::new(None));
    let Ok(mut provider) = provider.lock() else {
        debug!("sqlcmd telemetry provider lock is poisoned");
        return;
    };

    if provider.is_none() {
        let Ok(connection_string) = std::env::var(CONNECTION_STRING_ENV) else {
            return;
        };
        match production_provider(&connection_string) {
            Ok(configured) => *provider = Some(configured),
            Err(_) => {
                debug!("sqlcmd telemetry configuration is invalid");
                return;
            }
        }
    }

    let configured = provider.clone();
    drop(provider);
    if let Some(provider) = configured {
        emit_usage(&provider, sqlcmd_version, SystemTime::now());
    }
}

/// Flushes and shuts down telemetry within the requested bounded interval.
pub fn shutdown(timeout: Duration) -> FlushOutcome {
    if SHUTDOWN_STARTED.swap(true, Ordering::AcqRel) {
        return FlushOutcome::Completed;
    }
    let Some(provider) = PROVIDER.get() else {
        return FlushOutcome::Completed;
    };
    let Ok(mut provider) = provider.try_lock() else {
        return FlushOutcome::Failed;
    };
    let Some(provider) = provider.take() else {
        return FlushOutcome::Completed;
    };
    let timeout = timeout.min(MAX_FLUSH_TIMEOUT);
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("sqlcmd-telemetry-shutdown".to_owned())
        .spawn(move || {
            let outcome = if provider.shutdown_with_timeout(timeout).is_ok() {
                FlushOutcome::Completed
            } else {
                FlushOutcome::Failed
            };
            let _ = sender.send(outcome);
        });
    if worker.is_err() {
        return FlushOutcome::Failed;
    }
    match receiver.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(mpsc::RecvTimeoutError::Timeout) => FlushOutcome::TimedOut,
        Err(mpsc::RecvTimeoutError::Disconnected) => FlushOutcome::Failed,
    }
}

fn production_provider(
    connection_string: &str,
) -> Result<SdkTracerProvider, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(HTTP_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()?;
    provider_with_client(connection_string, client)
}

fn provider_with_client<C>(
    connection_string: &str,
    client: C,
) -> Result<SdkTracerProvider, Box<dyn std::error::Error + Send + Sync>>
where
    C: HttpClient + 'static,
{
    let exporter =
        Exporter::new_from_connection_string(connection_string, EventOnlyClient::new(client))?;
    let batch = BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(1)
                .with_max_export_batch_size(1)
                .with_scheduled_delay(MAX_FLUSH_TIMEOUT)
                .build(),
        )
        .build();
    Ok(SdkTracerProvider::builder()
        .with_span_processor(batch)
        .build())
}

fn emit_usage(provider: &SdkTracerProvider, sqlcmd_version: Option<&str>, timestamp: SystemTime) {
    let tracer = provider.tracer("mssql-sqlcmd");
    let mut span = tracer.start("sqlcmd.telemetry");
    let mut attributes = vec![
        KeyValue::new(CUSTOM_EVENT_NAME_ATTRIBUTE, USAGE_EVENT_NAME),
        KeyValue::new("sqlcmdcli_schema_version", SCHEMA_VERSION),
        KeyValue::new("sqlcmdcli_os", std::env::consts::OS),
        KeyValue::new("sqlcmdcli_arch", std::env::consts::ARCH),
        KeyValue::new("sqlcmdcli_implementation", IMPLEMENTATION),
        KeyValue::new("sqlcmdcli_runtime", RUNTIME),
    ];
    if let Some(version) = sqlcmd_version.filter(|version| version_is_safe(version)) {
        attributes.push(KeyValue::new(
            "sqlcmdcli_version",
            Value::String(version.to_owned().into()),
        ));
    }
    span.add_event_with_timestamp(CUSTOM_EVENT_OTEL_NAME, timestamp, attributes);
    span.end_with_timestamp(timestamp);
}

fn version_is_safe(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_VERSION_LEN
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    #[derive(Debug, Default)]
    struct InMemoryClient {
        requests: Mutex<Vec<Request<Bytes>>>,
        responses: Mutex<VecDeque<Result<Response<Bytes>, HttpError>>>,
    }

    #[derive(Clone, Debug)]
    struct SharedClient(Arc<InMemoryClient>);

    impl InMemoryClient {
        fn succeeding() -> SharedClient {
            let client = SharedClient(Arc::new(Self::default()));
            client
                .0
                .responses
                .lock()
                .expect("response lock")
                .push_back(Ok(Response::builder()
                    .status(200)
                    .body(Bytes::new())
                    .expect("valid response")));
            client
        }

        fn envelopes(&self) -> Vec<JsonValue> {
            let requests = self.requests.lock().expect("request lock");
            let request = requests.first().expect("one telemetry request");
            let mut decompressed = Vec::new();
            GzDecoder::new(request.body().as_ref())
                .read_to_end(&mut decompressed)
                .expect("gzip body");
            serde_json::from_slice(&decompressed).expect("JSON envelopes")
        }
    }

    #[async_trait]
    impl HttpClient for SharedClient {
        async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
            self.0.requests.lock().expect("request lock").push(request);
            self.0
                .responses
                .lock()
                .expect("response lock")
                .pop_front()
                .expect("configured response")
        }
    }

    const TEST_CONNECTION_STRING: &str = "InstrumentationKey=00000000-0000-0000-0000-000000000000;\
         IngestionEndpoint=https://example.test";

    #[test]
    fn exporter_sends_only_one_application_insights_event() {
        let client = InMemoryClient::succeeding();
        let provider = provider_with_client(TEST_CONNECTION_STRING, client.clone())
            .expect("valid test configuration");

        emit_usage(
            &provider,
            Some("18.5.1-preview.2"),
            UNIX_EPOCH + Duration::from_secs(1_730_736_000),
        );
        provider
            .shutdown_with_timeout(Duration::from_secs(1))
            .expect("bounded shutdown");

        let envelopes = client.0.envelopes();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(
            envelopes[0].get("name").and_then(JsonValue::as_str),
            Some(APPLICATION_INSIGHTS_EVENT_ENVELOPE)
        );
        assert_eq!(
            envelopes[0]
                .pointer("/data/baseData/name")
                .and_then(JsonValue::as_str),
            Some(USAGE_EVENT_NAME)
        );
        assert_eq!(
            envelopes[0]
                .pointer("/data/baseData/properties/sqlcmdcli_version")
                .and_then(JsonValue::as_str),
            Some("18.5.1-preview.2")
        );
        assert!(
            envelopes
                .iter()
                .all(|envelope| envelope.get("name").and_then(JsonValue::as_str)
                    != Some("Microsoft.ApplicationInsights.RemoteDependency"))
        );
    }

    #[test]
    fn invalid_connection_string_is_rejected_without_http() {
        let client = InMemoryClient::succeeding();
        assert!(provider_with_client("not a connection string", client).is_err());
    }

    #[test]
    fn unsafe_or_free_form_versions_are_not_exported() {
        for version in [
            "",
            "18.4 user@example.com",
            "18.4/server",
            "18.4\\path",
            "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        ] {
            assert!(!version_is_safe(version));
        }
        assert!(version_is_safe("18.5.1-preview.2"));
    }

    #[test]
    fn event_filter_drops_the_enclosing_dependency() {
        let body = serde_json::to_vec(&vec![
            serde_json::json!({"name": "Microsoft.ApplicationInsights.RemoteDependency"}),
            serde_json::json!({"name": APPLICATION_INSIGHTS_EVENT_ENVELOPE}),
        ])
        .expect("serialize envelopes");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&body).expect("compress envelopes");
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from(encoder.finish().expect("finish gzip")))
            .expect("valid request");

        let filtered = filter_event_request(request)
            .expect("filter succeeds")
            .expect("event remains");
        let mut decompressed = Vec::new();
        GzDecoder::new(filtered.request.body().as_ref())
            .read_to_end(&mut decompressed)
            .expect("decompress filtered body");
        let envelopes: Vec<JsonValue> =
            serde_json::from_slice(&decompressed).expect("filtered JSON");
        assert_eq!(envelopes.len(), 1);
        assert_eq!(
            envelopes[0].get("name").and_then(JsonValue::as_str),
            Some(APPLICATION_INSIGHTS_EVENT_ENVELOPE)
        );
    }

    #[test]
    fn partial_response_indices_are_mapped_to_the_original_exporter_batch() {
        let body = serde_json::to_vec(&vec![
            serde_json::json!({"name": "Microsoft.ApplicationInsights.RemoteDependency"}),
            serde_json::json!({"name": APPLICATION_INSIGHTS_EVENT_ENVELOPE}),
        ])
        .expect("serialize envelopes");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&body).expect("compress envelopes");
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from(encoder.finish().expect("finish gzip")))
            .expect("valid request");
        let filtered = filter_event_request(request)
            .expect("filter succeeds")
            .expect("event remains");
        let response = Response::builder()
            .status(206)
            .body(Bytes::from_static(
                br#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":0,"statusCode":429}]}"#,
            ))
            .expect("valid response");

        let translated = translate_partial_response(
            response,
            &filtered.original_indices,
            filtered.original_count,
        )
        .expect("translation succeeds");
        let payload: JsonValue =
            serde_json::from_slice(translated.body()).expect("translated JSON");
        assert_eq!(payload["itemsReceived"], 2);
        assert_eq!(payload["itemsAccepted"], 1);
        assert_eq!(payload["errors"][0]["index"], 1);
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Privacy-safe Application Insights usage telemetry for native sqlcmd.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{Read, Write};
use std::sync::Arc;
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
const EXPORT_SCHEDULE_DELAY: Duration = Duration::from_secs(60 * 60);

static PROVIDER: OnceLock<Mutex<Option<TelemetryProvider>>> = OnceLock::new();
static SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static ABI_TEST_CLIENT: OnceLock<Mutex<Option<mpsc::Sender<Request<Bytes>>>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushOutcome {
    Completed,
    Failed,
    TimedOut,
}

#[derive(Debug)]
struct EventOnlyClient<C> {
    inner: C,
    internal_failure: Arc<AtomicBool>,
}

#[cfg(test)]
#[derive(Clone, Debug)]
struct AbiTestClient(mpsc::Sender<Request<Bytes>>);

#[cfg(test)]
#[async_trait]
impl HttpClient for AbiTestClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        self.0
            .send(request)
            .map_err(|error| filter_error(error.to_string()))?;
        Ok(Response::builder()
            .status(200)
            .body(Bytes::new())
            .expect("valid test response"))
    }
}

impl<C> EventOnlyClient<C> {
    fn new(inner: C, internal_failure: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            internal_failure,
        }
    }
}

#[async_trait]
impl<C> HttpClient for EventOnlyClient<C>
where
    C: HttpClient,
{
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let filtered = match filter_event_request(request) {
            Ok(filtered) => filtered,
            Err(error) => {
                self.internal_failure.store(true, Ordering::Release);
                return Err(error);
            }
        };
        let FilteredRequest {
            request,
            original_indices,
            original_count,
        } = filtered;
        let response = self.inner.send_bytes(request).await?;
        match translate_partial_response(response, &original_indices, original_count) {
            Ok(response) => Ok(response),
            Err(error) => {
                self.internal_failure.store(true, Ordering::Release);
                Err(error)
            }
        }
    }
}

#[derive(Clone, Debug)]
struct TelemetryProvider {
    provider: SdkTracerProvider,
    internal_failure: Arc<AtomicBool>,
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

fn filter_event_request(request: Request<Bytes>) -> Result<FilteredRequest, HttpError> {
    let (parts, body) = request.into_parts();
    let mut decompressed = Vec::new();
    GzDecoder::new(body.as_ref())
        .read_to_end(&mut decompressed)
        .map_err(filter_error)?;

    let envelopes: Vec<JsonValue> = serde_json::from_slice(&decompressed).map_err(filter_error)?;
    let original_count = envelopes.len();
    let (original_indices, mut envelopes): (Vec<_>, Vec<_>) = envelopes
        .into_iter()
        .enumerate()
        .filter(|(_, envelope)| {
            envelope.get("name").and_then(JsonValue::as_str)
                == Some(APPLICATION_INSIGHTS_EVENT_ENVELOPE)
        })
        .unzip();
    if original_indices.is_empty() {
        return Err(filter_error(
            "Application Insights exporter payload contained no custom event",
        ));
    }
    for envelope in &mut envelopes {
        validate_and_sanitize_event(envelope)?;
    }

    let serialized = serde_json::to_vec(&envelopes).map_err(filter_error)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&serialized).map_err(filter_error)?;
    let compressed = encoder.finish().map_err(filter_error)?;
    Ok(FilteredRequest {
        request: Request::from_parts(parts, Bytes::from(compressed)),
        original_indices,
        original_count,
    })
}

fn validate_and_sanitize_event(envelope: &mut JsonValue) -> Result<(), HttpError> {
    if envelope
        .pointer("/data/baseData/name")
        .and_then(JsonValue::as_str)
        != Some(USAGE_EVENT_NAME)
    {
        return Err(filter_error(
            "Application Insights exporter payload contained an unexpected event",
        ));
    }

    let Some(properties) = envelope
        .pointer("/data/baseData/properties")
        .and_then(JsonValue::as_object)
    else {
        return Err(filter_error(
            "Application Insights usage event had no properties",
        ));
    };
    let required: BTreeSet<&str> = [
        "sqlcmdcli_schema_version",
        "sqlcmdcli_os",
        "sqlcmdcli_arch",
        "sqlcmdcli_implementation",
        "sqlcmdcli_runtime",
    ]
    .into_iter()
    .collect();
    let mut approved = required.clone();
    approved.insert("sqlcmdcli_version");
    if properties
        .keys()
        .any(|key| !approved.contains(key.as_str()))
        || required.iter().any(|key| !properties.contains_key(*key))
    {
        return Err(filter_error(
            "Application Insights usage event had unexpected properties",
        ));
    }

    envelope
        .as_object_mut()
        .ok_or_else(|| filter_error("Application Insights event envelope was not an object"))?
        .remove("tags");
    Ok(())
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
        emit_usage(&provider.provider, sqlcmd_version, SystemTime::now());
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
    shutdown_provider(provider, timeout)
}

fn shutdown_provider(provider: TelemetryProvider, timeout: Duration) -> FlushOutcome {
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("sqlcmd-telemetry-shutdown".to_owned())
        .spawn(move || {
            let outcome = if provider.provider.shutdown_with_timeout(timeout).is_err()
                || provider.internal_failure.load(Ordering::Acquire)
            {
                FlushOutcome::Failed
            } else {
                FlushOutcome::Completed
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
) -> Result<TelemetryProvider, Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(test)]
    if let Some(client) = ABI_TEST_CLIENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("ABI test client lock")
        .clone()
    {
        return provider_with_client(connection_string, AbiTestClient(client));
    }

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(HTTP_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()?;
    provider_with_client(connection_string, client)
}

fn provider_with_client<C>(
    connection_string: &str,
    client: C,
) -> Result<TelemetryProvider, Box<dyn std::error::Error + Send + Sync>>
where
    C: HttpClient + 'static,
{
    let internal_failure = Arc::new(AtomicBool::new(false));
    let exporter = Exporter::new_from_connection_string(
        connection_string,
        EventOnlyClient::new(client, Arc::clone(&internal_failure)),
    )?;
    let batch = BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(2)
                .with_max_export_batch_size(2)
                .with_scheduled_delay(EXPORT_SCHEDULE_DELAY)
                .build(),
        )
        .build();
    Ok(TelemetryProvider {
        provider: SdkTracerProvider::builder()
            .with_span_processor(batch)
            .build(),
        internal_failure,
    })
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
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::UNIX_EPOCH;

    #[derive(Debug, Default)]
    struct InMemoryClient {
        requests: Mutex<Vec<Request<Bytes>>>,
        responses: Mutex<VecDeque<Result<Response<Bytes>, HttpError>>>,
    }

    #[derive(Clone, Debug)]
    struct SharedClient(Arc<InMemoryClient>);

    #[derive(Debug)]
    struct SlowClient {
        delay: Duration,
    }

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

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

    #[async_trait]
    impl HttpClient for SlowClient {
        async fn send_bytes(&self, _request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
            std::thread::sleep(self.delay);
            Ok(Response::builder()
                .status(200)
                .body(Bytes::new())
                .expect("valid response"))
        }
    }

    const TEST_CONNECTION_STRING: &str = "InstrumentationKey=00000000-0000-0000-0000-000000000000;\
         IngestionEndpoint=https://example.test";

    fn poll_ready<F: Future>(future: F) -> F::Output {
        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future unexpectedly remained pending"),
        }
    }

    fn event_envelope() -> JsonValue {
        serde_json::json!({
            "name": APPLICATION_INSIGHTS_EVENT_ENVELOPE,
            "tags": {
                "ai.internal.sdkVersion": "opentelemetry0.33.1",
                "ai.operation.id": "0123456789abcdef0123456789abcdef"
            },
            "data": {
                "baseData": {
                    "name": USAGE_EVENT_NAME,
                    "properties": {
                        "sqlcmdcli_schema_version": "1",
                        "sqlcmdcli_version": "18.5",
                        "sqlcmdcli_os": "windows",
                        "sqlcmdcli_arch": "x86_64",
                        "sqlcmdcli_implementation": "mssql-sqlcmd-rust",
                        "sqlcmdcli_runtime": "native"
                    }
                }
            }
        })
    }

    #[test]
    fn exporter_sends_only_one_application_insights_event() {
        let client = InMemoryClient::succeeding();
        let provider = provider_with_client(TEST_CONNECTION_STRING, client.clone())
            .expect("valid test configuration");

        emit_usage(
            &provider.provider,
            Some("18.5.1-preview.2"),
            UNIX_EPOCH + Duration::from_secs(1_730_736_000),
        );
        provider
            .provider
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
        assert!(envelopes[0].get("tags").is_none());
        let property_keys: BTreeSet<_> = envelopes[0]
            .pointer("/data/baseData/properties")
            .and_then(JsonValue::as_object)
            .expect("event properties")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            property_keys,
            [
                "sqlcmdcli_arch",
                "sqlcmdcli_implementation",
                "sqlcmdcli_os",
                "sqlcmdcli_runtime",
                "sqlcmdcli_schema_version",
                "sqlcmdcli_version",
            ]
            .into_iter()
            .collect()
        );
        assert!(
            envelopes
                .iter()
                .all(|envelope| envelope.get("name").and_then(JsonValue::as_str)
                    != Some("Microsoft.ApplicationInsights.RemoteDependency"))
        );
    }

    #[test]
    fn native_abi_initializes_from_environment_and_absence_stays_off() {
        // SAFETY: this is the only test that mutates this process-wide variable,
        // and no other test exercises the global telemetry provider.
        unsafe { std::env::remove_var(CONNECTION_STRING_ENV) };
        // SAFETY: a null optional text is valid for this ABI.
        unsafe {
            crate::ffi::mssql_sqlcmd_telemetry_record_usage(crate::ffi::MssqlSqlcmdText {
                data: std::ptr::null(),
                len: 0,
            });
        }
        assert!(
            PROVIDER
                .get()
                .expect("provider slot")
                .lock()
                .expect("provider lock")
                .is_none()
        );

        // Invalid configuration remains off. The implementation logs only a
        // fixed diagnostic and never formats this value.
        // SAFETY: see the process-wide environment synchronization above.
        unsafe {
            std::env::set_var(
                CONNECTION_STRING_ENV,
                "InstrumentationKey=DO-NOT-LOG-THIS;broken",
            )
        };
        let version: Vec<u16> = "18.5".encode_utf16().collect();
        // SAFETY: the UTF-16 buffer remains valid for the duration of the call.
        unsafe {
            crate::ffi::mssql_sqlcmd_telemetry_record_usage(crate::ffi::MssqlSqlcmdText {
                data: version.as_ptr(),
                len: version.len(),
            });
        }
        assert!(
            PROVIDER
                .get()
                .expect("provider slot")
                .lock()
                .expect("provider lock")
                .is_none()
        );

        let (request_sender, request_receiver) = mpsc::channel();
        *ABI_TEST_CLIENT
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("ABI test client lock") = Some(request_sender);
        // SAFETY: see the process-wide environment synchronization above.
        unsafe { std::env::set_var(CONNECTION_STRING_ENV, TEST_CONNECTION_STRING) };
        // SAFETY: the UTF-16 buffer remains valid for the duration of the call.
        unsafe {
            crate::ffi::mssql_sqlcmd_telemetry_record_usage(crate::ffi::MssqlSqlcmdText {
                data: version.as_ptr(),
                len: version.len(),
            });
        }
        let shutdown_status = crate::ffi::mssql_sqlcmd_telemetry_shutdown(1_000);
        // SAFETY: see the process-wide environment synchronization above.
        unsafe { std::env::remove_var(CONNECTION_STRING_ENV) };
        *ABI_TEST_CLIENT
            .get()
            .expect("ABI test client slot")
            .lock()
            .expect("ABI test client lock") = None;

        assert_eq!(shutdown_status, 0);
        let request = request_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("exporter request through native ABI");
        let mut decompressed = Vec::new();
        GzDecoder::new(request.body().as_ref())
            .read_to_end(&mut decompressed)
            .expect("gzip exporter request");
        let envelopes: Vec<JsonValue> =
            serde_json::from_slice(&decompressed).expect("exporter JSON");
        assert_eq!(envelopes.len(), 1);
        assert_eq!(
            envelopes[0]
                .pointer("/data/baseData/name")
                .and_then(JsonValue::as_str),
            Some(USAGE_EVENT_NAME)
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
            event_envelope(),
        ])
        .expect("serialize envelopes");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&body).expect("compress envelopes");
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from(encoder.finish().expect("finish gzip")))
            .expect("valid request");

        let filtered = filter_event_request(request).expect("filter succeeds");
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
        assert!(envelopes[0].get("tags").is_none());
    }

    #[test]
    fn event_filter_rejects_an_all_filtered_payload() {
        let body = serde_json::to_vec(&vec![serde_json::json!({
            "name": "Microsoft.ApplicationInsights.RemoteDependency"
        })])
        .expect("serialize dependency");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&body).expect("compress dependency");
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from(encoder.finish().expect("finish gzip")))
            .expect("valid request");

        assert!(filter_event_request(request).is_err());
    }

    #[test]
    fn client_records_all_filtered_payload_as_an_internal_failure() {
        let body = serde_json::to_vec(&vec![serde_json::json!({
            "name": "Microsoft.ApplicationInsights.RemoteDependency"
        })])
        .expect("serialize dependency");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&body).expect("compress dependency");
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from(encoder.finish().expect("finish gzip")))
            .expect("valid request");
        let internal_failure = Arc::new(AtomicBool::new(false));
        let client =
            EventOnlyClient::new(InMemoryClient::succeeding(), Arc::clone(&internal_failure));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");

        assert!(runtime.block_on(client.send_bytes(request)).is_err());
        assert!(internal_failure.load(Ordering::Acquire));
    }

    #[test]
    fn event_filter_rejects_malformed_compressed_payload() {
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from_static(b"not gzip"))
            .expect("valid request");
        assert!(filter_event_request(request).is_err());
    }

    #[test]
    fn client_records_malformed_compression_as_an_internal_failure() {
        let internal_failure = Arc::new(AtomicBool::new(false));
        let client =
            EventOnlyClient::new(InMemoryClient::succeeding(), Arc::clone(&internal_failure));
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from_static(b"not gzip"))
            .expect("valid request");
        assert!(poll_ready(client.send_bytes(request)).is_err());
        assert!(internal_failure.load(Ordering::Acquire));
    }

    #[test]
    fn event_filter_rejects_wrong_name_and_properties() {
        for envelope in [
            {
                let mut envelope = event_envelope();
                envelope["data"]["baseData"]["name"] = JsonValue::from("unexpected");
                envelope
            },
            {
                let mut envelope = event_envelope();
                envelope["data"]["baseData"]["properties"]["unapproved"] = JsonValue::from("value");
                envelope
            },
            {
                let mut envelope = event_envelope();
                envelope["data"]["baseData"]["properties"]
                    .as_object_mut()
                    .expect("properties")
                    .remove("sqlcmdcli_runtime");
                envelope
            },
        ] {
            let body = serde_json::to_vec(&vec![envelope]).expect("serialize event");
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&body).expect("compress event");
            let request = Request::post("https://example.test/v2/track")
                .body(Bytes::from(encoder.finish().expect("finish gzip")))
                .expect("valid request");
            assert!(filter_event_request(request).is_err());
        }
    }

    #[test]
    fn partial_response_indices_are_mapped_to_the_original_exporter_batch() {
        let body = serde_json::to_vec(&vec![
            serde_json::json!({"name": "Microsoft.ApplicationInsights.RemoteDependency"}),
            event_envelope(),
        ])
        .expect("serialize envelopes");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&body).expect("compress envelopes");
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from(encoder.finish().expect("finish gzip")))
            .expect("valid request");
        let filtered = filter_event_request(request).expect("filter succeeds");
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

    #[test]
    fn malformed_partial_response_is_an_error() {
        let response = Response::builder()
            .status(206)
            .body(Bytes::from_static(b"not json"))
            .expect("valid response");

        assert!(translate_partial_response(response, &[1], 2).is_err());
    }

    #[test]
    fn client_records_malformed_partial_response_as_an_internal_failure() {
        let inner = InMemoryClient::succeeding();
        *inner.0.responses.lock().expect("response lock") =
            VecDeque::from([Ok(Response::builder()
                .status(206)
                .body(Bytes::from_static(b"not JSON"))
                .expect("valid response"))]);
        let internal_failure = Arc::new(AtomicBool::new(false));
        let client = EventOnlyClient::new(inner, Arc::clone(&internal_failure));

        let body = serde_json::to_vec(&vec![
            serde_json::json!({"name": "Microsoft.ApplicationInsights.RemoteDependency"}),
            event_envelope(),
        ])
        .expect("serialize envelopes");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&body).expect("compress envelopes");
        let request = Request::post("https://example.test/v2/track")
            .body(Bytes::from(encoder.finish().expect("finish gzip")))
            .expect("valid request");

        assert!(poll_ready(client.send_bytes(request)).is_err());
        assert!(internal_failure.load(Ordering::Acquire));
    }

    #[test]
    fn shutdown_times_out_while_the_http_request_is_still_running() {
        let provider = provider_with_client(
            TEST_CONNECTION_STRING,
            SlowClient {
                delay: Duration::from_secs(2),
            },
        )
        .expect("valid test configuration");
        emit_usage(&provider.provider, Some("18.5"), UNIX_EPOCH);
        let started = std::time::Instant::now();

        let outcome = shutdown_provider(provider, MAX_FLUSH_TIMEOUT);

        assert_eq!(outcome, FlushOutcome::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(1_500));
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Privacy-safe Application Insights usage telemetry for native sqlcmd.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
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

use crate::caller::{self, AiAgentSource};
use crate::device_id;
use crate::server_info::{self, RawServerInfo, ServerInfo};

pub const USAGE_EVENT_NAME: &str = "sqlcmdcli_usage";
/// The Application Insights connection string for the sqlcmd telemetry
/// resource. A build can embed one in `SQLCMD_TELEMETRY_CONNECTION_STRING_DEFAULT`;
/// this variable overrides it. Telemetry is off when there is neither.
pub const CONNECTION_STRING_ENV: &str = "SQLCMD_TELEMETRY_CONNECTION_STRING";
/// Set to `1`, `true` or `yes` to turn telemetry off.
pub const OPTOUT_ENV: &str = "SQLCMD_TELEMETRY_OPTOUT";
const BUILT_IN_CONNECTION_STRING: Option<&str> =
    option_env!("SQLCMD_TELEMETRY_CONNECTION_STRING_DEFAULT");

const CUSTOM_EVENT_OTEL_NAME: &str = "ai.custom";
const CUSTOM_EVENT_NAME_ATTRIBUTE: &str = "ai.customEvent.name";
const APPLICATION_INSIGHTS_EVENT_ENVELOPE: &str = "Microsoft.ApplicationInsights.Event";
const MAX_VERSION_LEN: usize = 64;
pub const MAX_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
const EXPORT_SCHEDULE_DELAY: Duration = Duration::from_secs(60 * 60);
/// How long the export thread may take; the process waits for it only as long as
/// the bound `shutdown` is given.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

pub const KEY_VERSION: &str = "sqlcmdcli_version";
pub const KEY_OS: &str = "sqlcmdcli_os";
pub const KEY_ARCH: &str = "sqlcmdcli_arch";
pub const KEY_COMMAND: &str = "sqlcmdcli_command";
pub const KEY_FORMAT: &str = "sqlcmdcli_format";
pub const KEY_DEVICE_ID: &str = "sqlcmdcli_devdeviceid";
pub const KEY_CALLER_TYPE: &str = "sqlcmdcli_caller_type";
pub const KEY_AI_AGENT_SOURCE: &str = "sqlcmdcli_ai_agent_source";

/// The properties an event may carry. Anything else fails the event.
const APPROVED_KEYS: [&str; 12] = [
    KEY_VERSION,
    KEY_OS,
    KEY_ARCH,
    KEY_COMMAND,
    KEY_FORMAT,
    KEY_DEVICE_ID,
    KEY_CALLER_TYPE,
    KEY_AI_AGENT_SOURCE,
    server_info::KEY_ENGINE_EDITION,
    server_info::KEY_SERVER_VERSION,
    server_info::KEY_SERVER_OS,
    server_info::KEY_SERVER_PLATFORM,
];
/// Present on every event.
const REQUIRED_KEYS: [&str; 2] = [KEY_OS, KEY_ARCH];

/// What the invocation asked for, as native sqlcmd reports it. Closed sets, so no
/// raw argument can come through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Not a command sqlcmd measures by name; the property is omitted.
    Unspecified,
    Diagnose,
}

impl Command {
    pub fn from_abi(value: i32) -> Self {
        match value {
            1 => Self::Diagnose,
            _ => Self::Unspecified,
        }
    }
}

/// How `--format` was given.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatUse {
    /// The option was not given: no property, and no "not used" value.
    Absent,
    Json,
    Unrecognized,
    Missing,
}

impl FormatUse {
    pub fn from_abi(value: i32) -> Self {
        match value {
            1 => Self::Json,
            2 => Self::Unrecognized,
            3 => Self::Missing,
            _ => Self::Absent,
        }
    }
}

/// One invocation, from native sqlcmd.
#[derive(Clone, Copy, Debug)]
pub struct Invocation<'a> {
    pub version: Option<&'a str>,
    pub command: Command,
    pub format: FormatUse,
}

/// One process's event, assembled as the run goes: the invocation first, the
/// server's properties when a connection succeeds. The first connection's
/// properties are the ones kept.
struct Session {
    base: Vec<(&'static str, String)>,
    server: Option<Vec<(&'static str, String)>>,
    export: Option<mpsc::Receiver<FlushOutcome>>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
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
static ABI_TEST_DELAY: Mutex<Duration> = Mutex::new(Duration::ZERO);

#[cfg(test)]
#[async_trait]
impl HttpClient for AbiTestClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let delay = *ABI_TEST_DELAY.lock().expect("delay lock");
        std::thread::sleep(delay);
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
    let approved: BTreeSet<&str> = APPROVED_KEYS.into_iter().collect();
    if properties.iter().any(|(key, value)| {
        !approved.contains(key.as_str())
            || !value
                .as_str()
                .is_some_and(|value| property_is_valid(key, value))
    }) || REQUIRED_KEYS
        .iter()
        .any(|key| !properties.contains_key(*key))
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

/// Whether the user turned telemetry off.
fn opted_out() -> bool {
    std::env::var(OPTOUT_ENV)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

/// The connection string telemetry would use: the variable, else the one built in.
fn connection_string() -> Option<String> {
    std::env::var(CONNECTION_STRING_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| BUILT_IN_CONNECTION_STRING.map(str::to_owned))
}

/// Whether this run reports telemetry: not opted out, and a destination exists.
/// Native sqlcmd reads the server properties only when this is true.
pub fn enabled() -> bool {
    !opted_out() && connection_string().is_some()
}

fn os_value() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "macos",
        _ => "other",
    }
}

fn arch_value() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "x86",
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => "other",
    }
}

/// Whether `value` is one `key` may carry. Applied to every property before an
/// event is built and again when the request is filtered.
fn property_is_valid(key: &str, value: &str) -> bool {
    match key {
        KEY_VERSION => version_is_safe(value),
        KEY_OS => matches!(value, "windows" | "linux" | "macos" | "other"),
        KEY_ARCH => matches!(value, "x86" | "x86_64" | "aarch64" | "other"),
        KEY_COMMAND => value == "diagnose",
        KEY_FORMAT => matches!(value, "json" | "unrecognized" | "missing"),
        KEY_DEVICE_ID => device_id::normalize(value).as_deref() == Some(value),
        KEY_CALLER_TYPE => matches!(
            value,
            "human" | "agent" | "pipeline" | "other_automation" | "unknown"
        ),
        KEY_AI_AGENT_SOURCE => matches!(
            value,
            "GitHubCopilotVsCode" | "ClaudeCode" | "OpenAICodex" | "GeminiCli"
        ),
        other => server_info::value_is_valid(other, value),
    }
}

/// The properties known when the invocation starts.
fn base_properties(invocation: &Invocation<'_>) -> Vec<(&'static str, String)> {
    let mut properties: Vec<(&'static str, String)> = Vec::new();
    if let Some(version) = invocation
        .version
        .filter(|version| version_is_safe(version))
    {
        properties.push((KEY_VERSION, version.to_owned()));
    }
    properties.push((KEY_OS, os_value().to_owned()));
    properties.push((KEY_ARCH, arch_value().to_owned()));
    if invocation.command == Command::Diagnose {
        properties.push((KEY_COMMAND, "diagnose".to_owned()));
    }
    match invocation.format {
        FormatUse::Absent => {}
        FormatUse::Json => properties.push((KEY_FORMAT, "json".to_owned())),
        FormatUse::Unrecognized => properties.push((KEY_FORMAT, "unrecognized".to_owned())),
        FormatUse::Missing => properties.push((KEY_FORMAT, "missing".to_owned())),
    }
    let found = caller::detect();
    properties.push((KEY_CALLER_TYPE, found.caller_type.as_str().to_owned()));
    if found.agent != AiAgentSource::None {
        properties.push((KEY_AI_AGENT_SOURCE, found.agent.as_str().to_owned()));
    }
    // Last, so nothing above reads the device ID's store before it is known that
    // this run reports at all.
    if let Some(id) = device_id_value() {
        properties.push((KEY_DEVICE_ID, id));
    }
    properties.retain(|(key, value)| property_is_valid(key, value));
    properties
}

#[cfg(not(test))]
fn device_id_value() -> Option<String> {
    device_id::get_or_create()
}

/// Unit tests must not read or write the developer's real shared ID.
#[cfg(test)]
fn device_id_value() -> Option<String> {
    Some(TEST_DEVICE_ID.to_owned())
}

#[cfg(test)]
const TEST_DEVICE_ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

/// Records what this run is, for its one usage event.
///
/// Does nothing unless [`enabled`]. Never fails, and never affects sqlcmd.
pub fn record_usage(invocation: Invocation<'_>) {
    if SHUTDOWN_STARTED.load(Ordering::Acquire) || !enabled() {
        return;
    }
    let Ok(mut session) = SESSION.lock() else {
        debug!("sqlcmd telemetry session lock is poisoned");
        return;
    };
    if session.is_some() {
        return;
    }
    *session = Some(Session {
        base: base_properties(&invocation),
        server: None,
        export: None,
    });
}

/// Adds the server's properties from the first successful connection and sends
/// the event now, so the network work overlaps what sqlcmd does next. Later
/// connections are ignored.
pub fn record_server_info(raw: &RawServerInfo) {
    if SHUTDOWN_STARTED.load(Ordering::Acquire) {
        return;
    }
    let Ok(mut guard) = SESSION.lock() else {
        return;
    };
    let Some(session) = guard.as_mut() else {
        return;
    };
    if session.server.is_some() {
        return;
    }
    let mut properties = ServerInfo::classify(raw).properties();
    properties.retain(|(key, value)| property_is_valid(key, value));
    session.server = Some(properties);
    start_export(session);
}

/// Starts sending the event on its own thread, once.
fn start_export(session: &mut Session) {
    if session.export.is_some() {
        return;
    }
    let mut properties = session.base.clone();
    if let Some(server) = &session.server {
        properties.extend(server.iter().cloned());
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("sqlcmd-telemetry".to_owned())
        .spawn(move || {
            let _ = sender.send(export_once(&properties));
        });
    if spawned.is_ok() {
        session.export = Some(receiver);
    }
}

fn export_once(properties: &[(&'static str, String)]) -> FlushOutcome {
    let Some(connection_string) = connection_string() else {
        return FlushOutcome::Failed;
    };
    let Ok(provider) = production_provider(&connection_string) else {
        debug!("sqlcmd telemetry configuration is invalid");
        return FlushOutcome::Failed;
    };
    emit_usage(&provider.provider, properties, SystemTime::now());
    if provider
        .provider
        .shutdown_with_timeout(EXPORT_TIMEOUT)
        .is_err()
        || provider.internal_failure.load(Ordering::Acquire)
    {
        FlushOutcome::Failed
    } else {
        FlushOutcome::Completed
    }
}

/// Sends the event if it has not been sent, and waits for it within `timeout`
/// (capped at [`MAX_FLUSH_TIMEOUT`]).
pub fn shutdown(timeout: Duration) -> FlushOutcome {
    if SHUTDOWN_STARTED.swap(true, Ordering::AcqRel) {
        return FlushOutcome::Completed;
    }
    let Ok(mut guard) = SESSION.try_lock() else {
        return FlushOutcome::Failed;
    };
    let Some(mut session) = guard.take() else {
        return FlushOutcome::Completed;
    };
    drop(guard);
    start_export(&mut session);
    let Some(receiver) = session.export else {
        return FlushOutcome::Failed;
    };
    match receiver.recv_timeout(timeout.min(MAX_FLUSH_TIMEOUT)) {
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

fn emit_usage(
    provider: &SdkTracerProvider,
    properties: &[(&'static str, String)],
    timestamp: SystemTime,
) {
    let tracer = provider.tracer("mssql-sqlcmd");
    let mut span = tracer.start("sqlcmd.telemetry");
    let mut attributes = vec![KeyValue::new(CUSTOM_EVENT_NAME_ATTRIBUTE, USAGE_EVENT_NAME)];
    attributes.extend(
        properties
            .iter()
            .map(|(key, value)| KeyValue::new(*key, Value::String(value.clone().into()))),
    );
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
                        "sqlcmdcli_version": "18.5",
                        "sqlcmdcli_os": "windows",
                        "sqlcmdcli_arch": "x86_64",
                        "sqlcmdcli_command": "diagnose",
                        "sqlcmdcli_caller_type": "unknown"
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
            &[
                (KEY_VERSION, "18.5.1-preview.2".to_owned()),
                (KEY_OS, "windows".to_owned()),
                (KEY_ARCH, "x86_64".to_owned()),
            ],
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
            ["sqlcmdcli_arch", "sqlcmdcli_os", "sqlcmdcli_version",]
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
    fn native_abi_flow_sends_one_event_with_the_server_properties_early() {
        let _guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let requests = _guard.capture_requests();
        let version: Vec<u16> = "18.5".encode_utf16().collect();
        // SAFETY: the UTF-16 buffer remains valid for the duration of the call.
        unsafe {
            crate::ffi::mssql_sqlcmd_telemetry_record_usage(
                crate::ffi::MssqlSqlcmdText {
                    data: version.as_ptr(),
                    len: version.len(),
                },
                1,
                1,
            );
        }
        let info = server_info_fixture();
        // SAFETY: the fixture's texts outlive the call.
        let status = unsafe { crate::ffi::mssql_sqlcmd_telemetry_record_connection(&info.raw) };
        assert_eq!(status, 0);

        // The event goes out when the connection is recorded, before shutdown.
        let request = requests
            .recv_timeout(Duration::from_secs(2))
            .expect("event sent after the connection was recorded");
        assert_eq!(crate::ffi::mssql_sqlcmd_telemetry_shutdown(1_000), 0);

        let properties = event_properties(&request);
        assert_eq!(properties["sqlcmdcli_version"], "18.5");
        assert_eq!(properties[KEY_COMMAND], "diagnose");
        assert_eq!(properties[KEY_FORMAT], "json");
        assert_eq!(properties[KEY_DEVICE_ID], TEST_DEVICE_ID);
        assert!(matches!(
            properties[KEY_CALLER_TYPE].as_str(),
            "agent" | "pipeline" | "unknown"
        ));
        assert_eq!(properties[server_info::KEY_ENGINE_EDITION], "2");
        assert_eq!(properties[server_info::KEY_SERVER_OS], "windows");
        assert_eq!(properties[server_info::KEY_SERVER_PLATFORM], "NT x64");
        assert!(requests.try_recv().is_err(), "exactly one request");
    }

    #[test]
    fn shutdown_without_a_connection_still_sends_the_invocation_event() {
        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let requests = guard.capture_requests();
        record_usage(Invocation {
            version: Some("18.5"),
            command: Command::Unspecified,
            format: FormatUse::Absent,
        });

        assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Completed);

        let properties = event_properties(&requests.try_recv().expect("one request"));
        assert!(!properties.contains_key(KEY_COMMAND));
        assert!(!properties.contains_key(KEY_FORMAT));
        assert!(!properties.contains_key(server_info::KEY_ENGINE_EDITION));
        assert!(properties.contains_key(KEY_OS) && properties.contains_key(KEY_ARCH));
    }

    #[test]
    fn the_first_connection_wins() {
        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let requests = guard.capture_requests();
        record_usage(Invocation {
            version: None,
            command: Command::Unspecified,
            format: FormatUse::Absent,
        });
        record_server_info(&RawServerInfo {
            engine_edition: 5,
            ..RawServerInfo::default()
        });
        record_server_info(&RawServerInfo {
            engine_edition: 2,
            ..RawServerInfo::default()
        });
        assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Completed);

        let properties = event_properties(&requests.try_recv().expect("one request"));
        assert_eq!(properties[server_info::KEY_ENGINE_EDITION], "5");
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn opt_out_and_a_missing_destination_send_nothing_and_read_no_device_id() {
        for (connection_string, opt_out) in [(Some(TEST_CONNECTION_STRING), true), (None, false)] {
            let guard = TestEnv::new(connection_string);
            let requests = guard.capture_requests();
            if opt_out {
                // SAFETY: serialized by the test environment lock.
                unsafe { std::env::set_var(OPTOUT_ENV, "1") };
            }
            assert!(!enabled());
            record_usage(Invocation {
                version: Some("18.5"),
                command: Command::Diagnose,
                format: FormatUse::Json,
            });
            record_server_info(&server_info_fixture().parsed);
            assert!(SESSION.lock().expect("session lock").is_none());
            assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Completed);
            assert!(requests.try_recv().is_err());
        }
    }

    #[test]
    fn opt_out_values() {
        let _guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("0", false),
            ("", false),
        ] {
            // SAFETY: serialized by the test environment lock.
            unsafe { std::env::set_var(OPTOUT_ENV, value) };
            assert_eq!(opted_out(), expected, "{value:?}");
        }
    }

    #[test]
    fn invalid_connection_string_stays_off() {
        let guard = TestEnv::new(Some("InstrumentationKey=DO-NOT-LOG-THIS;broken"));
        let requests = guard.capture_requests();
        record_usage(Invocation {
            version: Some("18.5"),
            command: Command::Unspecified,
            format: FormatUse::Absent,
        });
        assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Failed);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn shutdown_times_out_while_the_http_request_is_still_running() {
        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let _requests = guard.capture_requests();
        *ABI_TEST_DELAY.lock().expect("delay lock") = Duration::from_secs(2);
        record_usage(Invocation {
            version: Some("18.5"),
            command: Command::Unspecified,
            format: FormatUse::Absent,
        });
        let started = std::time::Instant::now();

        let outcome = shutdown(MAX_FLUSH_TIMEOUT);
        *ABI_TEST_DELAY.lock().expect("delay lock") = Duration::ZERO;

        assert_eq!(outcome, FlushOutcome::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(1_500));
    }

    #[test]
    fn values_outside_the_approved_sets_are_rejected() {
        for (key, value) in [
            (KEY_OS, "plan9"),
            (KEY_ARCH, "mips"),
            (KEY_COMMAND, "query"),
            (KEY_FORMAT, "xml"),
            (KEY_DEVICE_ID, "not-a-uuid"),
            (KEY_DEVICE_ID, "0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D"),
            (KEY_CALLER_TYPE, "robot"),
            (KEY_AI_AGENT_SOURCE, "SomethingElse"),
            (server_info::KEY_ENGINE_EDITION, "999999"),
            (KEY_VERSION, "18.4 user@example.com"),
        ] {
            assert!(!property_is_valid(key, value), "{key}={value}");
        }
        for (key, value) in [
            (KEY_OS, "macos"),
            (KEY_ARCH, "aarch64"),
            (KEY_COMMAND, "diagnose"),
            (KEY_FORMAT, "missing"),
            (KEY_DEVICE_ID, TEST_DEVICE_ID),
            (KEY_CALLER_TYPE, "pipeline"),
            (KEY_CALLER_TYPE, "human"),
            (KEY_CALLER_TYPE, "other_automation"),
            (KEY_AI_AGENT_SOURCE, "ClaudeCode"),
        ] {
            assert!(property_is_valid(key, value), "{key}={value}");
        }
    }

    /// Serializes the tests that use the process-wide session and environment,
    /// and puts both back as they were.
    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    impl TestEnv {
        fn new(connection_string: Option<&str>) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // SAFETY: serialized by ENV_LOCK; these variables are read only by
            // the telemetry module.
            unsafe {
                std::env::remove_var(OPTOUT_ENV);
                match connection_string {
                    Some(value) => std::env::set_var(CONNECTION_STRING_ENV, value),
                    None => std::env::remove_var(CONNECTION_STRING_ENV),
                }
            }
            *SESSION.lock().expect("session lock") = None;
            SHUTDOWN_STARTED.store(false, Ordering::Release);
            *ABI_TEST_DELAY.lock().expect("delay lock") = Duration::ZERO;
            Self { _lock: lock }
        }

        /// Sends what the exporter would post to the returned channel instead.
        fn capture_requests(&self) -> mpsc::Receiver<Request<Bytes>> {
            let (sender, receiver) = mpsc::channel();
            *ABI_TEST_CLIENT
                .get_or_init(|| Mutex::new(None))
                .lock()
                .expect("ABI test client lock") = Some(sender);
            receiver
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            // SAFETY: still holding ENV_LOCK.
            unsafe {
                std::env::remove_var(OPTOUT_ENV);
                std::env::remove_var(CONNECTION_STRING_ENV);
            }
            *SESSION
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            if let Some(slot) = ABI_TEST_CLIENT.get() {
                *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
        }
    }

    struct ServerInfoFixture {
        raw: crate::ffi::MssqlSqlcmdServerInfo,
        parsed: RawServerInfo,
        _buffers: Vec<Vec<u16>>,
    }

    /// A local SQL Server 2025 on Windows, as the metadata query returns it.
    fn server_info_fixture() -> ServerInfoFixture {
        let texts: Vec<Vec<u16>> = ["17.0.5015.4", "windows", "x64"]
            .iter()
            .map(|text| text.encode_utf16().collect())
            .collect();
        let text = |index: usize| crate::ffi::MssqlSqlcmdText {
            data: texts[index].as_ptr(),
            len: texts[index].len(),
        };
        let raw = crate::ffi::MssqlSqlcmdServerInfo {
            engine_edition: 2,
            product_version: text(0),
            os: text(1),
            arch: text(2),
        };
        let parsed = RawServerInfo {
            engine_edition: 2,
            product_version: Some("17.0.5015.4".into()),
            os: Some("windows".into()),
            arch: Some("x64".into()),
        };
        ServerInfoFixture {
            raw,
            parsed,
            _buffers: texts,
        }
    }

    fn event_properties(request: &Request<Bytes>) -> std::collections::BTreeMap<String, String> {
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
        envelopes[0]
            .pointer("/data/baseData/properties")
            .and_then(JsonValue::as_object)
            .expect("event properties")
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().expect("string").to_owned()))
            .collect()
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
                    .remove("sqlcmdcli_os");
                envelope
            },
            {
                let mut envelope = event_envelope();
                envelope["data"]["baseData"]["properties"]["sqlcmdcli_format"] =
                    JsonValue::from("xml");
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
}

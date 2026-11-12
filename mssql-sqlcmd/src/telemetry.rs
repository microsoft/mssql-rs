// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Privacy-safe Application Insights usage telemetry for native sqlcmd.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

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
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use serde_json::Value as JsonValue;
use tracing::debug;

use crate::caller;
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
/// The event is one span with one span event; a little room beyond that.
const MAX_EVENTS_PER_SPAN: u32 = 8;
pub const MAX_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

pub const KEY_VERSION: &str = "sqlcmdcli_version";
pub const KEY_OS: &str = "sqlcmdcli_os";
pub const KEY_ARCH: &str = "sqlcmdcli_arch";
pub const KEY_COMMAND: &str = "sqlcmdcli_command";
pub const KEY_FORMAT: &str = "sqlcmdcli_format";
pub const KEY_DEVICE_ID: &str = "sqlcmdcli_devdeviceid";
pub const KEY_CALLER_TYPE: &str = "sqlcmdcli_caller_type";

/// The properties an event may carry. Anything else fails the event.
const APPROVED_KEYS: [&str; 11] = [
    KEY_VERSION,
    KEY_OS,
    KEY_ARCH,
    KEY_COMMAND,
    KEY_FORMAT,
    KEY_DEVICE_ID,
    KEY_CALLER_TYPE,
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
            crate::ffi::MSSQL_SQLCMD_COMMAND_DIAGNOSE => Self::Diagnose,
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
            crate::ffi::MSSQL_SQLCMD_FORMAT_JSON => Self::Json,
            crate::ffi::MSSQL_SQLCMD_FORMAT_UNRECOGNIZED => Self::Unrecognized,
            crate::ffi::MSSQL_SQLCMD_FORMAT_MISSING => Self::Missing,
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
static ABI_TEST_CLIENT: Mutex<Option<mpsc::Sender<Request<Bytes>>>> = Mutex::new(None);

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

/// The status and body the test client answers with.
#[cfg(test)]
static ABI_TEST_RESPONSE: Mutex<(u16, &'static str)> = Mutex::new((200, ""));

#[cfg(test)]
#[async_trait]
impl HttpClient for AbiTestClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let delay = *ABI_TEST_DELAY.lock().expect("delay lock");
        std::thread::sleep(delay);
        self.0
            .send(request)
            .map_err(|error| filter_error(error.to_string()))?;
        let (status, body) = *ABI_TEST_RESPONSE.lock().expect("response lock");
        Ok(Response::builder()
            .status(status)
            .body(Bytes::from_static(body.as_bytes()))
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
        let response = match self.inner.send_bytes(request).await {
            Ok(response) => response,
            Err(error) => {
                self.internal_failure.store(true, Ordering::Release);
                return Err(error);
            }
        };
        // The exporter retries throttling and server errors itself, and its
        // shutdown does not hand the result back, so the outcome is kept here.
        // Each attempt overwrites the last, so the attempt the exporter stops on
        // decides: a 503 that a retry answers with 200 is a delivery.
        match check_ingestion_response(response, &original_indices, original_count) {
            Ok((response, accepted)) => {
                self.internal_failure.store(!accepted, Ordering::Release);
                Ok(response)
            }
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
    if properties.iter().any(|(key, value)| {
        !APPROVED_KEYS.contains(&key.as_str())
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

/// Checks an ingestion response against what was sent, translates a partial
/// one back to the exporter's original batch, and says whether the service
/// accepted the event, as the exporter judges it: only a 200, or a 206 in which
/// every item was accepted. Any other status (a 201 or 204 as well as a 4xx or
/// 5xx) is a failure to the exporter, so it is not accepted here. A malformed
/// response is an error and never an apparent success.
fn check_ingestion_response(
    response: Response<Bytes>,
    original_indices: &[usize],
    original_count: usize,
) -> Result<(Response<Bytes>, bool), HttpError> {
    let status = response.status();
    if !matches!(status.as_u16(), 206 | 500) {
        let accepted = status.as_u16() == 200;
        return Ok((response, accepted));
    }
    let (parts, body) = response.into_parts();
    let mut payload: JsonValue = serde_json::from_slice(&body).map_err(filter_error)?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| filter_error("Application Insights response was not an object"))?;
    let sent = original_indices.len() as u64;
    let items_received = object.get("itemsReceived").and_then(JsonValue::as_u64);
    let items_accepted = object.get("itemsAccepted").and_then(JsonValue::as_u64);
    let (Some(items_received), Some(items_accepted)) = (items_received, items_accepted) else {
        return Err(filter_error(
            "Application Insights response had no item counts",
        ));
    };
    if items_received != sent || items_accepted > items_received {
        return Err(filter_error(
            "Application Insights response counts do not match the request",
        ));
    }

    let mut seen = BTreeSet::new();
    let mut error_count = 0u64;
    // The exporter requires the array, even an empty one.
    {
        let errors = object
            .get_mut("errors")
            .and_then(JsonValue::as_array_mut)
            .ok_or_else(|| filter_error("Application Insights response had no errors list"))?;
        for error in errors {
            let filtered_index = error
                .get("index")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| filter_error("Application Insights response error had no index"))?;
            let original_index = usize::try_from(filtered_index)
                .ok()
                .and_then(|index| original_indices.get(index))
                .ok_or_else(|| filter_error("invalid Application Insights response index"))?;
            if !seen.insert(filtered_index) {
                return Err(filter_error(
                    "duplicate Application Insights response index",
                ));
            }
            error_count += 1;
            error["index"] = JsonValue::from(*original_index);
        }
    }
    if items_accepted + error_count > items_received {
        return Err(filter_error(
            "Application Insights response counts are inconsistent",
        ));
    }
    // A 500 is a rejection even when its body parses; a 206 is one if any item
    // was rejected.
    let accepted = status.as_u16() == 206 && error_count == 0 && items_accepted == items_received;

    let dropped_count = original_count.saturating_sub(original_indices.len()) as u64;
    object.insert("itemsReceived".to_owned(), JsonValue::from(original_count));
    object.insert(
        "itemsAccepted".to_owned(),
        JsonValue::from(items_accepted + dropped_count),
    );
    let body = serde_json::to_vec(&payload).map_err(filter_error)?;
    Ok((Response::from_parts(parts, Bytes::from(body)), accepted))
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

/// A client that only lets the exporter parse a connection string.
#[derive(Debug)]
struct ValidationClient;

#[async_trait]
impl HttpClient for ValidationClient {
    async fn send_bytes(&self, _request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        Err(filter_error("the validation client sends nothing"))
    }
}

/// Whether the exporter accepts `connection_string` and its instrumentation key
/// is a GUID. The exporter does not check the key's format, so a mistyped key
/// would otherwise enable collection that the service then rejects.
fn connection_string_is_valid(connection_string: &str) -> bool {
    Exporter::new_from_connection_string(connection_string, ValidationClient).is_ok()
        && instrumentation_key_is_guid(connection_string)
}

/// Reads the key the way the exporter does, so the key checked is the key used:
/// names are case-insensitive, nothing is trimmed, and when a name repeats the
/// last one wins. The exporter has already rejected any part that is not a
/// single `name=value`.
fn instrumentation_key_is_guid(connection_string: &str) -> bool {
    connection_string
        .split(';')
        .filter_map(|part| part.split_once('='))
        .rfind(|(name, _)| name.eq_ignore_ascii_case("InstrumentationKey"))
        .is_some_and(|(_, key)| {
            let groups: Vec<&str> = key.split('-').collect();
            groups.len() == 5
                && groups.iter().zip([8, 4, 4, 4, 12]).all(|(group, len)| {
                    group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit())
                })
        })
}

/// The destination this run would send to, and only if the exporter accepts it.
fn destination() -> Option<String> {
    destination_from(
        std::env::var(CONNECTION_STRING_ENV),
        BUILT_IN_CONNECTION_STRING,
    )
}

/// [`destination`] with the variable and the built-in connection string given.
/// The built-in one is used only when the variable is absent: a variable that is
/// set, even to an empty or invalid value (or one that is not valid text), turns
/// telemetry off and does not fall back.
fn destination_from(
    variable: Result<String, std::env::VarError>,
    built_in: Option<&str>,
) -> Option<String> {
    let candidate = match variable {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => built_in?.to_owned(),
        Err(std::env::VarError::NotUnicode(_)) => return None,
    };
    connection_string_is_valid(&candidate).then_some(candidate)
}

/// Whether this run reports telemetry: not opted out, and a valid destination
/// exists. Everything with a side effect (the device ID, the server query) waits
/// for this, so an invalid configuration is a no-op.
pub fn enabled() -> bool {
    !opted_out() && destination().is_some()
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
        other => server_info::value_is_valid(other, value),
    }
}

/// The properties known when the invocation starts.
///
/// This reads or creates the shared device ID, so it must only be called once
/// [`enabled`] is true; [`record_usage`] checks that first.
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
    properties.push((KEY_CALLER_TYPE, caller::detect().as_str().to_owned()));
    // Last, after everything that needs no stored state.
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

/// Unit tests must not read or write the developer's real shared ID. They count
/// the reads, to show where the ID is not touched.
#[cfg(test)]
fn device_id_value() -> Option<String> {
    DEVICE_ID_READS.fetch_add(1, Ordering::AcqRel);
    Some(TEST_DEVICE_ID.to_owned())
}

#[cfg(test)]
static DEVICE_ID_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
const TEST_DEVICE_ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

/// Records what this run is, for its one usage event.
///
/// Does nothing unless [`enabled`]. Never fails, and never affects sqlcmd.
pub fn record_usage(invocation: Invocation<'_>) {
    if !enabled() {
        return;
    }
    // Built before the lock is taken: it can read the device ID's store.
    let base = base_properties(&invocation);
    let Ok(mut session) = SESSION.lock() else {
        debug!("sqlcmd telemetry session lock is poisoned");
        return;
    };
    // Checked under the lock, which shutdown also holds when it sets the flag,
    // so a call that is admitted here is seen by shutdown, and one that comes
    // after it is not recorded.
    if SHUTDOWN_STARTED.load(Ordering::Acquire) || session.is_some() {
        return;
    }
    *session = Some(Session {
        base,
        server: None,
        export: None,
    });
}

/// Adds the server's properties from the first successful connection and sends
/// the event now, so the network work overlaps what sqlcmd does next. Later
/// connections are ignored.
pub fn record_server_info(raw: &RawServerInfo) {
    let Ok(mut guard) = SESSION.lock() else {
        return;
    };
    if SHUTDOWN_STARTED.load(Ordering::Acquire) {
        return;
    }
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
///
/// The thread exports the one event when its span ends, then shuts its provider
/// down, so nothing waits on a timer.
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
    let Some(connection_string) = destination() else {
        return FlushOutcome::Failed;
    };
    let Ok(provider) = production_provider(&connection_string) else {
        debug!("sqlcmd telemetry configuration is invalid");
        return FlushOutcome::Failed;
    };
    // The simple exporter has already sent the event, retries included, when the
    // span ended; shutting down only releases the provider.
    emit_usage(&provider.provider, properties, SystemTime::now());
    if provider.provider.shutdown().is_err() || provider.internal_failure.load(Ordering::Acquire) {
        FlushOutcome::Failed
    } else {
        FlushOutcome::Completed
    }
}

/// Sends the event if it has not been sent, and waits for it within `timeout`
/// (capped at [`MAX_FLUSH_TIMEOUT`]).
///
/// The session lock is taken with a bounded wait, never a blocking one, because
/// this can run on the Windows console-control thread while another thread holds
/// the lock. It is not async-signal-safe (it takes locks, allocates and spawns a
/// thread): never call it from a POSIX signal handler.
pub fn shutdown(timeout: Duration) -> FlushOutcome {
    let timeout = timeout.min(MAX_FLUSH_TIMEOUT);
    let deadline = Instant::now() + timeout;
    let mut guard = loop {
        match SESSION.try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return FlushOutcome::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    };
    if SHUTDOWN_STARTED.swap(true, Ordering::AcqRel) {
        return FlushOutcome::Completed;
    }
    let Some(mut session) = guard.take() else {
        return FlushOutcome::Completed;
    };
    drop(guard);
    start_export(&mut session);
    let Some(receiver) = session.export else {
        return FlushOutcome::Failed;
    };
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
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
    // One event, exported when its span ends, on this thread: nothing is queued
    // behind a timer. The SDK reads OTEL_* variables for its defaults, so the
    // resource, the sampler and the event limit are set here so that ambient
    // tracing configuration can neither add identifiers to the event nor switch
    // it off.
    Ok(TelemetryProvider {
        provider: SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .with_resource(Resource::builder_empty().build())
            .with_sampler(Sampler::AlwaysOn)
            .with_max_events_per_span(MAX_EVENTS_PER_SPAN)
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

/// A native sqlcmd version: two to four numeric parts, then optionally a `-` or
/// `+` suffix of letters, digits, `.`, `-` and `+` (`18.7.0001.1`,
/// `18.5.1-preview.2`). Anything else, such as a free-form label, is not sent.
fn version_is_safe(version: &str) -> bool {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        return false;
    }
    let (core, suffix) = match version.find(['-', '+']) {
        Some(at) => (&version[..at], Some(&version[at + 1..])),
        None => (version, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    let core_ok = (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    let suffix_ok = suffix.is_none_or(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    });
    core_ok && suffix_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::future::Future;
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
        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let requests = guard.capture_requests();
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
        assert_eq!(properties[KEY_CALLER_TYPE], "pipeline");
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
            // A build with a compiled-in destination keeps it when the variable
            // is not set, so there the "missing destination" case is enabled.
            let built_in = BUILT_IN_CONNECTION_STRING.is_some_and(connection_string_is_valid);
            if !opt_out && built_in {
                assert!(enabled());
                continue;
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
            assert_eq!(DEVICE_ID_READS.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn the_built_in_destination_is_used_only_when_the_variable_is_absent() {
        use std::env::VarError;
        use std::ffi::OsString;
        let other = "InstrumentationKey=11111111-1111-1111-1111-111111111111;\
                     IngestionEndpoint=https://other.test";
        let built_in = Some(TEST_CONNECTION_STRING);

        // Absent: the built-in one, if there is one.
        assert_eq!(
            destination_from(Err(VarError::NotPresent), built_in).as_deref(),
            built_in
        );
        assert_eq!(destination_from(Err(VarError::NotPresent), None), None);

        // A valid variable replaces it.
        assert_eq!(
            destination_from(Ok(other.to_owned()), built_in).as_deref(),
            Some(other)
        );

        // A variable that is set, even to nothing, is an override: it does not
        // fall back to the built-in destination.
        for explicit in ["", "   ", "\t", "not a connection string"] {
            assert_eq!(
                destination_from(Ok(explicit.to_owned()), built_in),
                None,
                "{explicit:?}"
            );
        }
        assert_eq!(
            destination_from(Err(VarError::NotUnicode(OsString::from("x"))), built_in),
            None
        );
    }

    #[test]
    fn this_builds_destination_follows_the_environment() {
        let _guard = TestEnv::new(None);
        assert_eq!(
            enabled(),
            BUILT_IN_CONNECTION_STRING.is_some_and(connection_string_is_valid)
        );
        // SAFETY: serialized by the test environment lock.
        unsafe { std::env::set_var(CONNECTION_STRING_ENV, "") };
        assert!(!enabled());
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
    fn an_invalid_connection_string_is_a_no_op_with_no_side_effects() {
        for invalid in [
            "InstrumentationKey=DO-NOT-LOG-THIS;broken",
            "InstrumentationKey=broken",
            "InstrumentationKey=00000000-0000-0000-0000-00000000000",
            "InstrumentationKey=00000000-0000-0000-0000-00000000000g",
            // The exporter uses the last key and does not trim it.
            "InstrumentationKey=00000000-0000-0000-0000-000000000000;InstrumentationKey=broken",
            "InstrumentationKey= 00000000-0000-0000-0000-000000000000",
            "InstrumentationKey=00000000-0000-0000-0000-000000000000 ",
            "IngestionEndpoint=https://example.invalid/",
            "not a connection string",
            "   x   ",
            "",
            "   ",
        ] {
            let guard = TestEnv::new(Some(invalid));
            let requests = guard.capture_requests();
            assert!(!enabled(), "{invalid:?}");
            record_usage(Invocation {
                version: Some("18.5"),
                command: Command::Diagnose,
                format: FormatUse::Json,
            });
            record_server_info(&server_info_fixture().parsed);
            assert!(SESSION.lock().expect("session lock").is_none());
            assert_eq!(DEVICE_ID_READS.load(Ordering::Acquire), 0, "{invalid:?}");
            assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Completed);
            assert!(requests.try_recv().is_err());
        }
    }

    #[test]
    fn a_valid_connection_string_is_enabled() {
        let _guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        assert!(enabled());
    }

    /// What a row of the table below expects from `shutdown`.
    #[derive(Clone, Copy, Debug)]
    enum Expect {
        /// The service accepted the event.
        Completed,
        /// Rejected outright, with no retry.
        Failed,
        /// Throttled, a server error, or any error the exporter retries (a
        /// transport failure, a filter failure, a response that does not add
        /// up): it retries with a backoff, so a short wait ends first. Never a
        /// success.
        NotCompleted,
    }

    /// The service's answer decides the outcome the C ABI reports: a rejection is
    /// a failure even though the SDK's shutdown does not hand the result back.
    #[test]
    fn a_rejected_event_is_reported_as_a_failure() {
        use Expect::{Completed, Failed, NotCompleted};
        for (status, body, expect) in [
            (200, "", Completed),
            (
                206,
                r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#,
                Completed,
            ),
            (403, "", Failed),
            (204, "", Failed),
            (400, r#"{"errors":[]}"#, Failed),
            (429, "", NotCompleted),
            (500, "", NotCompleted),
            (503, "", NotCompleted),
            (
                206,
                r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":0,"statusCode":429}]}"#,
                NotCompleted,
            ),
            (206, "not json", NotCompleted),
        ] {
            let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
            let requests = guard.capture_requests();
            guard.respond_with(status, body);
            record_usage(Invocation {
                version: Some("18.5"),
                command: Command::Unspecified,
                format: FormatUse::Absent,
            });

            let wait = match expect {
                NotCompleted => Duration::from_millis(300),
                Completed | Failed => Duration::from_secs(1),
            };
            let outcome = shutdown(wait);

            assert!(requests.try_recv().is_ok(), "the event was sent: {status}");
            match expect {
                Completed => assert_eq!(outcome, FlushOutcome::Completed, "{status} {body}"),
                Failed => assert_eq!(outcome, FlushOutcome::Failed, "{status} {body}"),
                NotCompleted => assert_ne!(outcome, FlushOutcome::Completed, "{status} {body}"),
            }
        }
    }

    /// The same decision at the level of one response: which are accepted.
    #[test]
    fn a_response_is_accepted_only_if_the_service_took_the_event() {
        let response = |status: u16, body: &'static str| {
            Response::builder()
                .status(status)
                .body(Bytes::from_static(body.as_bytes()))
                .expect("valid response")
        };
        let accepted = |status: u16, body: &'static str| {
            check_ingestion_response(response(status, body), &[0], 1).map(|(_, accepted)| accepted)
        };
        assert!(accepted(200, "").expect("ok"));
        assert!(accepted(206, r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#).expect("ok"));
        // The exporter treats every status but 200 and 206 as a failure, so a
        // 2xx other than those is not an acceptance either.
        for rejected in [201, 202, 204, 301, 400, 401, 403, 408, 429, 503] {
            assert!(
                !accepted(rejected, "").expect("passed through"),
                "{rejected}"
            );
        }
        assert!(
            !accepted(
                206,
                r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":0}]}"#
            )
            .expect("ok")
        );
        // A 500 is a rejection even with a well-formed body.
        assert!(
            !accepted(
                500,
                r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":0}]}"#
            )
            .expect("ok")
        );
        // ...and even when the body reports every item accepted.
        assert!(
            !accepted(500, r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#).expect("ok")
        );
        // A malformed one is an error, not an acceptance.
        assert!(accepted(206, "[]").is_err());
    }

    /// The exporter retries, so the attempt it stops on decides: a rejection that
    /// a retry turns into a delivery is not a failure, and the reverse is.
    #[test]
    fn the_last_attempt_decides_the_outcome() {
        let reply = |status: u16| {
            Ok(Response::builder()
                .status(status)
                .body(Bytes::new())
                .expect("valid response"))
        };
        for (attempts, failed) in [
            (vec![503, 200], false),
            (vec![429, 429, 200], false),
            (vec![200, 503], true),
            (vec![503, 503], true),
            (vec![403], true),
            (vec![200], false),
        ] {
            let inner = InMemoryClient::succeeding();
            *inner.0.responses.lock().expect("response lock") =
                attempts.iter().map(|status| reply(*status)).collect();
            let internal_failure = Arc::new(AtomicBool::new(false));
            let client = EventOnlyClient::new(inner, Arc::clone(&internal_failure));
            for _ in &attempts {
                let request = track_request(&[event_envelope()]);
                poll_ready(client.send_bytes(request)).expect("the response is passed on");
            }
            assert_eq!(
                internal_failure.load(Ordering::Acquire),
                failed,
                "{attempts:?}"
            );
        }
    }

    #[test]
    fn a_transport_error_that_a_retry_recovers_from_is_a_delivery() {
        let ok = || {
            Ok(Response::builder()
                .status(200)
                .body(Bytes::new())
                .expect("valid response"))
        };
        // An error that a retry recovers from is a delivery.
        let inner = InMemoryClient::succeeding();
        *inner.0.responses.lock().expect("response lock") =
            VecDeque::from([Err(filter_error("connection reset")), ok()]);
        let internal_failure = Arc::new(AtomicBool::new(false));
        let client = EventOnlyClient::new(inner, Arc::clone(&internal_failure));
        assert!(poll_ready(client.send_bytes(track_request(&[event_envelope()]))).is_err());
        assert!(internal_failure.load(Ordering::Acquire));
        assert!(poll_ready(client.send_bytes(track_request(&[event_envelope()]))).is_ok());
        assert!(!internal_failure.load(Ordering::Acquire));
    }

    /// A call that is admitted before shutdown is sent by it, and one that comes
    /// after is not recorded: never a recorded session that nobody sends.
    #[test]
    fn recording_racing_shutdown_never_leaves_an_unsent_session() {
        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let requests = guard.capture_requests();
        for _ in 0..40 {
            *SESSION.lock().expect("session lock") = None;
            SHUTDOWN_STARTED.store(false, Ordering::Release);
            while requests.try_recv().is_ok() {}

            // Both callers wait on the session lock; which goes first is up to
            // the scheduler, and either order must be consistent.
            let held = SESSION.lock().expect("session lock");
            let recorder = std::thread::spawn(|| {
                record_usage(Invocation {
                    version: Some("18.5"),
                    command: Command::Unspecified,
                    format: FormatUse::Absent,
                });
            });
            let closer = std::thread::spawn(|| shutdown(Duration::from_secs(1)));
            std::thread::sleep(Duration::from_millis(5));
            drop(held);
            recorder.join().expect("recorder");
            let outcome = closer.join().expect("closer");

            assert!(SESSION.lock().expect("session lock").is_none());
            // Recorded first: shutdown sent it. Shutdown first: it was not
            // recorded. Either way at most one event, and none left over.
            if requests.recv_timeout(Duration::from_millis(50)).is_ok() {
                assert_eq!(outcome, FlushOutcome::Completed);
            }
            assert!(requests.try_recv().is_err(), "at most one event");
        }
    }

    #[test]
    fn nothing_is_recorded_after_shutdown() {
        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let requests = guard.capture_requests();
        assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Completed);
        record_usage(Invocation {
            version: Some("18.5"),
            command: Command::Unspecified,
            format: FormatUse::Absent,
        });
        record_server_info(&server_info_fixture().parsed);
        assert!(SESSION.lock().expect("session lock").is_none());
        assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Completed);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn shutdown_does_not_block_forever_when_the_session_lock_is_held() {
        let _guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let held = SESSION.lock().expect("session lock");
        let started = Instant::now();

        let outcome = shutdown(Duration::from_millis(200));

        assert_eq!(outcome, FlushOutcome::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(800));
        assert!(!SHUTDOWN_STARTED.load(Ordering::Acquire));
        drop(held);
    }

    /// Ambient tracing configuration must neither switch the event off nor add
    /// identifiers to it.
    #[test]
    fn ambient_opentelemetry_settings_do_not_change_the_event() {
        let _guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let vars = [
            ("OTEL_TRACES_SAMPLER", "always_off"),
            ("OTEL_SPAN_EVENT_COUNT_LIMIT", "0"),
            ("OTEL_SERVICE_NAME", "someone-elses-service"),
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "device.id=abc,ai.user.id=someone",
            ),
        ];
        for (name, value) in vars {
            // SAFETY: serialized by the test environment lock.
            unsafe { std::env::set_var(name, value) };
        }
        let client = InMemoryClient::succeeding();
        let provider = provider_with_client(TEST_CONNECTION_STRING, client.clone())
            .expect("valid test configuration");
        emit_usage(
            &provider.provider,
            &[
                (KEY_OS, "windows".to_owned()),
                (KEY_ARCH, "x86_64".to_owned()),
            ],
            UNIX_EPOCH + Duration::from_secs(1_730_736_000),
        );
        provider
            .provider
            .shutdown_with_timeout(Duration::from_secs(1))
            .expect("bounded shutdown");
        for (name, _) in vars {
            // SAFETY: as above.
            unsafe { std::env::remove_var(name) };
        }

        let envelopes = client.0.envelopes();
        assert_eq!(envelopes.len(), 1, "the event was not switched off");
        assert!(envelopes[0].get("tags").is_none());
        let text = serde_json::to_string(&envelopes[0]).expect("envelope text");
        for leaked in [
            "someone-elses-service",
            "device.id",
            "ai.user.id",
            "someone",
        ] {
            assert!(!text.contains(leaked), "{leaked}");
        }
        assert!(!provider.internal_failure.load(Ordering::Acquire));
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
            ("sqlcmdcli_ai_agent_source", "ClaudeCode"),
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
        ] {
            assert!(property_is_valid(key, value), "{key}={value}");
        }
    }

    /// Serializes the tests that use the process-wide session and environment,
    /// and clears them afterwards.
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
                // A declaration wins over every environment marker, so the
                // caller type does not depend on where the tests run.
                std::env::set_var(caller::DECLARED_CALLER_ENV, "pipeline");
                match connection_string {
                    Some(value) => std::env::set_var(CONNECTION_STRING_ENV, value),
                    None => std::env::remove_var(CONNECTION_STRING_ENV),
                }
            }
            *SESSION.lock().unwrap_or_else(|p| p.into_inner()) = None;
            SHUTDOWN_STARTED.store(false, Ordering::Release);
            DEVICE_ID_READS.store(0, Ordering::Release);
            *ABI_TEST_DELAY.lock().expect("delay lock") = Duration::ZERO;
            *ABI_TEST_RESPONSE.lock().expect("response lock") = (200, "");
            Self { _lock: lock }
        }

        /// Sends what the exporter would post to the returned channel instead.
        fn capture_requests(&self) -> mpsc::Receiver<Request<Bytes>> {
            let (sender, receiver) = mpsc::channel();
            *ABI_TEST_CLIENT.lock().expect("ABI test client lock") = Some(sender);
            receiver
        }

        /// What the service answers with, for the next requests.
        fn respond_with(&self, status: u16, body: &'static str) {
            *ABI_TEST_RESPONSE.lock().expect("response lock") = (status, body);
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            // SAFETY: still holding ENV_LOCK.
            unsafe {
                std::env::remove_var(OPTOUT_ENV);
                std::env::remove_var(CONNECTION_STRING_ENV);
                std::env::remove_var(caller::DECLARED_CALLER_ENV);
            }
            *SESSION
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            *ABI_TEST_CLIENT
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            *ABI_TEST_RESPONSE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = (200, "");
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
            "not-a-version",
            "---",
            "abc",
            "18",
            "v18.5",
            "18..5",
            ".18.5",
            "18.5.",
            "18.5-",
            "-18.5",
            "18.5.1.2.3",
            "18.5_beta",
            "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        ] {
            assert!(!version_is_safe(version), "{version:?}");
        }
        for version in [
            "18.5",
            "18.7.0001.1",
            "18.5.1-preview.2",
            "18.5.1+build.7",
            "18.5.1-rc-1",
        ] {
            assert!(version_is_safe(version), "{version:?}");
        }
    }

    fn gzip_json(envelopes: &[JsonValue]) -> Vec<u8> {
        let body = serde_json::to_vec(envelopes).expect("serialize envelopes");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&body).expect("compress envelopes");
        encoder.finish().expect("finish gzip")
    }

    /// A request to the track endpoint with the given body.
    fn track_request_with(body: Bytes) -> Request<Bytes> {
        Request::post("https://example.test/v2/track")
            .body(body)
            .expect("valid request")
    }

    /// A request to the track endpoint carrying `envelopes`, compressed as the
    /// exporter sends them.
    fn track_request(envelopes: &[JsonValue]) -> Request<Bytes> {
        track_request_with(Bytes::from(gzip_json(envelopes)))
    }

    /// The envelope of the span the exporter always adds around the event.
    fn dependency_envelope() -> JsonValue {
        serde_json::json!({"name": "Microsoft.ApplicationInsights.RemoteDependency"})
    }

    #[test]
    fn event_filter_drops_the_enclosing_dependency() {
        let request = track_request(&[dependency_envelope(), event_envelope()]);

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
        assert!(filter_event_request(track_request(&[dependency_envelope()])).is_err());
    }

    #[test]
    fn client_records_all_filtered_payload_as_an_internal_failure() {
        let request = track_request(&[dependency_envelope()]);
        let internal_failure = Arc::new(AtomicBool::new(false));
        let client =
            EventOnlyClient::new(InMemoryClient::succeeding(), Arc::clone(&internal_failure));

        assert!(poll_ready(client.send_bytes(request)).is_err());
        assert!(internal_failure.load(Ordering::Acquire));
    }

    #[test]
    fn client_records_a_transport_failure_as_an_internal_failure() {
        let inner = InMemoryClient::succeeding();
        *inner.0.responses.lock().expect("response lock") =
            VecDeque::from([Err(filter_error("connection refused"))]);
        let internal_failure = Arc::new(AtomicBool::new(false));
        let client = EventOnlyClient::new(inner, Arc::clone(&internal_failure));

        assert!(poll_ready(client.send_bytes(track_request(&[event_envelope()]))).is_err());
        assert!(internal_failure.load(Ordering::Acquire));
    }

    #[test]
    fn client_records_a_rejection_as_an_internal_failure_but_passes_the_response_on() {
        let inner = InMemoryClient::succeeding();
        *inner.0.responses.lock().expect("response lock") =
            VecDeque::from([Ok(Response::builder()
                .status(403)
                .body(Bytes::new())
                .expect("valid response"))]);
        let internal_failure = Arc::new(AtomicBool::new(false));
        let client = EventOnlyClient::new(inner, Arc::clone(&internal_failure));

        let response = poll_ready(client.send_bytes(track_request(&[event_envelope()])))
            .expect("the exporter still sees the response");
        assert_eq!(response.status().as_u16(), 403);
        assert!(internal_failure.load(Ordering::Acquire));
    }

    #[test]
    fn event_filter_rejects_malformed_compressed_payload() {
        assert!(filter_event_request(track_request_with(Bytes::from_static(b"not gzip"))).is_err());
    }

    #[test]
    fn client_records_malformed_compression_as_an_internal_failure() {
        let internal_failure = Arc::new(AtomicBool::new(false));
        let client =
            EventOnlyClient::new(InMemoryClient::succeeding(), Arc::clone(&internal_failure));
        let request = track_request_with(Bytes::from_static(b"not gzip"));
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
            assert!(filter_event_request(track_request(&[envelope])).is_err());
        }
    }

    #[test]
    fn partial_response_indices_are_mapped_to_the_original_exporter_batch() {
        let request = track_request(&[dependency_envelope(), event_envelope()]);
        let filtered = filter_event_request(request).expect("filter succeeds");
        let response = Response::builder()
            .status(206)
            .body(Bytes::from_static(
                br#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":0,"statusCode":429}]}"#,
            ))
            .expect("valid response");

        let (translated, accepted) = check_ingestion_response(
            response,
            &filtered.original_indices,
            filtered.original_count,
        )
        .expect("translation succeeds");
        assert!(!accepted, "the one retained event was rejected");
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

        assert!(check_ingestion_response(response, &[1], 2).is_err());
    }

    #[test]
    fn a_partial_response_that_does_not_match_the_request_is_an_error_not_a_success() {
        // One event was retained out of two envelopes (index 1 of the original).
        for body in [
            r#"{"itemsReceived":0,"itemsAccepted":1,"errors":[]}"#,
            r#"{"itemsReceived":2,"itemsAccepted":2,"errors":[]}"#,
            r#"{"itemsReceived":1,"itemsAccepted":2,"errors":[]}"#,
            r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[{"index":0}]}"#,
            r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":5}]}"#,
            r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":0},{"index":0}]}"#,
            r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"statusCode":429}]}"#,
            r#"{"itemsReceived":1,"itemsAccepted":0,"errors":"none"}"#,
            r#"{"itemsReceived":1,"itemsAccepted":1}"#,
            r#"{"itemsReceived":1,"itemsAccepted":0,"errors":null}"#,
            r#"{"itemsAccepted":0,"errors":[]}"#,
            r#"{"itemsReceived":"1","itemsAccepted":0}"#,
            r#"[]"#,
            r#""text""#,
            r#"7"#,
            r#"null"#,
        ] {
            let response = Response::builder()
                .status(206)
                .body(Bytes::from(body))
                .expect("valid response");
            assert!(
                check_ingestion_response(response, &[1], 2).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn a_consistent_partial_response_without_errors_is_translated() {
        let response = Response::builder()
            .status(206)
            .body(Bytes::from_static(
                br#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#,
            ))
            .expect("valid response");

        let (translated, accepted) =
            check_ingestion_response(response, &[1], 2).expect("consistent");
        assert!(accepted);
        let payload: JsonValue = serde_json::from_slice(translated.body()).expect("JSON");
        assert_eq!(payload["itemsReceived"], 2);
        assert_eq!(payload["itemsAccepted"], 2);
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

        let request = track_request(&[dependency_envelope(), event_envelope()]);

        assert!(poll_ready(client.send_bytes(request)).is_err());
        assert!(internal_failure.load(Ordering::Acquire));
    }
}

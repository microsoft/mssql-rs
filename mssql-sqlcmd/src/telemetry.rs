// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Privacy-safe Application Insights usage telemetry for native sqlcmd.

use std::sync::Mutex;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use tracing::debug;

use crate::caller;
use crate::device_id;
use crate::ingestion::{self, Destination, HttpPost, Post};
use crate::server_info::{self, RawServerInfo, ServerInfo};

pub const USAGE_EVENT_NAME: &str = "sqlcmdcli_usage";
/// The Application Insights connection string for the sqlcmd telemetry
/// resource. `SQLCMD_TELEMETRY_CONNECTION_STRING` overrides the built-in one
/// (for testing, or to send somewhere else), and an override that is empty or
/// invalid turns telemetry off.
pub const CONNECTION_STRING_ENV: &str = "SQLCMD_TELEMETRY_CONNECTION_STRING";
/// Set to `1`, `true` or `yes` to turn telemetry off.
pub const OPTOUT_ENV: &str = "SQLCMD_TELEMETRY_OPTOUT";

/// The destination every build sends to unless the variable above says
/// otherwise: the SQLCMD resource DataX created for this product. An
/// instrumentation key can only send events, never read them, and client tools
/// ship it for that reason; it is not a secret. Only the key and the ingestion
/// endpoint are kept, since sending needs nothing else.
const DEFAULT_CONNECTION_STRING: &str = "InstrumentationKey=0e88a50a-3564-4c2b-9b20-9e9b0e8af3d4;\
     IngestionEndpoint=https://centralus-2.in.applicationinsights.azure.com/";

/// What telemetry uses when the variable is absent. Unit tests have none, so
/// that no test can send to the real resource; each test that needs a
/// destination sets the variable or passes one in.
#[cfg(not(test))]
const BUILT_IN_CONNECTION_STRING: Option<&str> = Some(DEFAULT_CONNECTION_STRING);
#[cfg(test)]
const BUILT_IN_CONNECTION_STRING: Option<&str> = None;

/// The longest sqlcmd version accepted. A valid version is ASCII, so this is its
/// length both in UTF-8 bytes (`version_is_safe`) and in UTF-16 code units (the
/// C ABI's check before it reads the text).
pub(crate) const MAX_VERSION_LEN: usize = 64;
/// The longest the process waits for the event at exit, whatever the caller
/// asks for. Half a second keeps a short run (`sqlcmd -Q "SELECT 1"`) from being
/// held up by a cold send; an event that is not delivered by then is dropped.
pub const MAX_FLUSH_TIMEOUT: Duration = Duration::from_millis(500);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

pub const KEY_VERSION: &str = "sqlcmdcli_version";
pub const KEY_OS: &str = "sqlcmdcli_os";
pub const KEY_ARCH: &str = "sqlcmdcli_arch";
pub const KEY_COMMAND: &str = "sqlcmdcli_command";
pub const KEY_FORMAT: &str = "sqlcmdcli_format";
pub const KEY_DEVICE_ID: &str = "sqlcmdcli_devdeviceid";
pub const KEY_CALLER_TYPE: &str = "sqlcmdcli_caller_type";

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
    pub const fn from_abi(value: i32) -> Self {
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
    pub const fn from_abi(value: i32) -> Self {
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

/// What a test transport was asked to send.
#[cfg(test)]
struct Captured {
    url: String,
    body: Vec<u8>,
}

#[cfg(test)]
static ABI_TEST_CLIENT: Mutex<Option<mpsc::Sender<Captured>>> = Mutex::new(None);

#[cfg(test)]
static ABI_TEST_DELAY: Mutex<Duration> = Mutex::new(Duration::ZERO);

/// The status and body the test transport answers with; status 0 is a transport
/// error.
#[cfg(test)]
static ABI_TEST_RESPONSE: Mutex<(u16, &'static str)> = Mutex::new((200, ""));

#[cfg(test)]
struct TestPost(mpsc::Sender<Captured>);

#[cfg(test)]
impl Post for TestPost {
    fn post(&self, url: &reqwest::Url, body: Vec<u8>) -> Result<ingestion::Reply, String> {
        let delay = *ABI_TEST_DELAY.lock().expect("delay lock");
        std::thread::sleep(delay);
        self.0
            .send(Captured {
                url: url.to_string(),
                body,
            })
            .map_err(|error| error.to_string())?;
        let (status, body) = *ABI_TEST_RESPONSE.lock().expect("response lock");
        if status == 0 {
            return Err("test transport error".to_owned());
        }
        Ok(ingestion::Reply {
            status,
            body: body.as_bytes().to_vec(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushOutcome {
    Completed,
    Failed,
    TimedOut,
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

/// Whether `connection_string` names a destination that can deliver: its
/// instrumentation key is a GUID and its ingestion endpoint is an HTTPS URL with
/// a host. A mistyped key or endpoint would otherwise enable collection (the
/// device ID, the server query) that can never be delivered.
fn connection_string_is_valid(connection_string: &str) -> bool {
    Destination::parse(connection_string).is_some()
}

/// The destination this run would send to, and only if it can deliver.
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

/// Whether this run reports telemetry: not opted out, a valid destination
/// exists, and the transport can run. Everything with a side effect (the device
/// ID, the server query) waits for this, so an invalid configuration, or a host
/// that cannot send, is a no-op.
pub fn enabled() -> bool {
    enabled_with(BUILT_IN_CONNECTION_STRING)
}

/// [`enabled`] with the built-in destination given, so a test can run the whole
/// gate with the shipped string without it ever entering the environment.
fn enabled_with(built_in: Option<&str>) -> bool {
    !opted_out()
        && destination_from(std::env::var(CONNECTION_STRING_ENV), built_in).is_some()
        && transport_available()
}

/// The HTTP client needs OpenSSL on 64-bit Linux. Checked without the report
/// `openssl_runtime` writes, so telemetry adds no output of its own, and after
/// the destination, so that a run with telemetry off never loads it. Tests send
/// through their own transport; `TRANSPORT_MISSING` stands in for a host without
/// OpenSSL and `TRANSPORT_CHECKS` counts the calls, so a test can see that a run
/// with telemetry off never gets here.
fn transport_available() -> bool {
    #[cfg(test)]
    {
        TRANSPORT_CHECKS.fetch_add(1, Ordering::AcqRel);
        if TRANSPORT_MISSING.load(Ordering::Acquire) {
            return false;
        }
    }
    #[cfg(all(not(test), target_pointer_width = "64", target_os = "linux"))]
    if !crate::openssl_runtime::is_available() {
        return false;
    }
    true
}

#[cfg(test)]
static TRANSPORT_MISSING: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static TRANSPORT_CHECKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
/// event is built and again on the final list before it is serialized.
///
/// This is also the allowlist of keys: a key that is not matched here falls to
/// `server_info::value_is_valid`, whose default is `false`, so an unlisted key
/// is never valid. There is deliberately no second list of keys to keep in step.
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

/// Sends the one event, once, and reports whether the service took it. There is
/// no retry: the process waits for this at most [`MAX_FLUSH_TIMEOUT`].
fn export_once(properties: &[(&'static str, String)]) -> FlushOutcome {
    let Some(destination) = destination().as_deref().and_then(Destination::parse) else {
        return FlushOutcome::Failed;
    };
    let Some(body) = usage_body(&destination, properties, SystemTime::now()) else {
        return FlushOutcome::Failed;
    };
    let Some(post) = production_post() else {
        debug!("sqlcmd telemetry transport could not be created");
        return FlushOutcome::Failed;
    };
    if ingestion::deliver(&*post, &destination, body) {
        FlushOutcome::Completed
    } else {
        FlushOutcome::Failed
    }
}

/// The request body for the usage event, only if every property is one that may
/// be sent and the ones every event has are present. This is the last check
/// before anything leaves the process, on the list that will be serialized.
fn usage_body(
    destination: &Destination,
    properties: &[(&'static str, String)],
    time: SystemTime,
) -> Option<Vec<u8>> {
    let allowed = properties
        .iter()
        .all(|(key, value)| property_is_valid(key, value))
        && REQUIRED_KEYS
            .iter()
            .all(|required| properties.iter().any(|(key, _)| key == required));
    if !allowed {
        debug!("sqlcmd telemetry event had unexpected properties");
        return None;
    }
    destination.event_body(USAGE_EVENT_NAME, properties, time)
}

fn production_post() -> Option<Box<dyn Post>> {
    #[cfg(test)]
    if let Some(sender) = ABI_TEST_CLIENT
        .lock()
        .expect("ABI test client lock")
        .clone()
    {
        return Some(Box::new(TestPost(sender)));
    }

    HttpPost::new(HTTP_TIMEOUT)
        .ok()
        .map(|post| Box::new(post) as Box<dyn Post>)
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
    use serde_json::Value as JsonValue;

    const TEST_CONNECTION_STRING: &str = "InstrumentationKey=00000000-0000-0000-0000-000000000000;\
         IngestionEndpoint=https://example.test";

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

        assert_eq!(request.url, "https://example.test/v2/track");
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

    /// A second `record_usage` is ignored: before the connection, the first
    /// invocation's properties are the ones sent; after the export has started,
    /// no second event is sent.
    #[test]
    fn the_first_invocation_wins() {
        let invocation = |version, format| Invocation {
            version: Some(version),
            command: Command::Unspecified,
            format,
        };

        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let requests = guard.capture_requests();
        record_usage(invocation("18.5", FormatUse::Json));
        record_usage(invocation("18.6", FormatUse::Missing));
        record_server_info(&RawServerInfo::default());
        // The export has started; a later invocation must not start another.
        record_usage(invocation("18.7", FormatUse::Unrecognized));
        assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Completed);

        let properties = event_properties(&requests.try_recv().expect("one request"));
        assert_eq!(properties[KEY_VERSION], "18.5");
        assert_eq!(properties[KEY_FORMAT], "json");
        assert!(requests.try_recv().is_err(), "one event only");
    }

    #[test]
    fn opt_out_and_a_missing_destination_send_nothing_and_read_no_device_id() {
        for (connection_string, opt_out) in [(Some(TEST_CONNECTION_STRING), true), (None, false)] {
            let guard = TestEnv::new(connection_string);
            if opt_out {
                // SAFETY: serialized by the test environment lock.
                unsafe { std::env::set_var(OPTOUT_ENV, "1") };
            }
            // Unit tests have no built-in destination (see
            // BUILT_IN_CONNECTION_STRING), so with no variable it is off.
            guard.assert_run_is_a_no_op(&format!("opt_out={opt_out}"));
            assert_eq!(TRANSPORT_CHECKS.load(Ordering::Acquire), 0);
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

    /// The destination built into every release is a valid one.
    #[test]
    fn the_shipped_default_destination_is_valid() {
        assert!(connection_string_is_valid(DEFAULT_CONNECTION_STRING));
        // The shipped string is only the key and the endpoint, and is not one of
        // the test fixtures.
        assert_ne!(DEFAULT_CONNECTION_STRING, TEST_CONNECTION_STRING);
        assert!(!DEFAULT_CONNECTION_STRING.contains("LiveEndpoint"));
    }

    /// The opt-out wins over the built-in destination: the whole gate runs with
    /// the shipped string, and nothing is sent.
    #[test]
    fn opting_out_turns_the_default_destination_off() {
        let _guard = TestEnv::new(None);
        assert!(enabled_with(Some(DEFAULT_CONNECTION_STRING)));
        // SAFETY: serialized by the test environment lock.
        unsafe { std::env::set_var(OPTOUT_ENV, "1") };
        assert!(!enabled_with(Some(DEFAULT_CONNECTION_STRING)));
    }

    #[test]
    fn without_a_variable_a_test_has_no_destination() {
        let _guard = TestEnv::new(None);
        assert!(!enabled());
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
            // The last key is the one used, and nothing is trimmed.
            "InstrumentationKey=00000000-0000-0000-0000-000000000000;InstrumentationKey=broken",
            "InstrumentationKey= 00000000-0000-0000-0000-000000000000",
            "InstrumentationKey=00000000-0000-0000-0000-000000000000 ",
            "IngestionEndpoint=https://example.invalid/",
            // A valid key with an endpoint that can never be reached.
            "InstrumentationKey=00000000-0000-0000-0000-000000000000;IngestionEndpoint=localhost",
            "InstrumentationKey=00000000-0000-0000-0000-000000000000;IngestionEndpoint=ftp://example.test",
            "InstrumentationKey=00000000-0000-0000-0000-000000000000;IngestionEndpoint=http://example.test",
            "InstrumentationKey=00000000-0000-0000-0000-000000000000;IngestionEndpoint=https://",
            "InstrumentationKey=00000000-0000-0000-0000-000000000000;IngestionEndpoint=https:///path",
            "InstrumentationKey=00000000-0000-0000-0000-000000000000;IngestionEndpoint=https://example.test;IngestionEndpoint=localhost",
            "not a connection string",
            "   x   ",
            "",
            "   ",
        ] {
            let guard = TestEnv::new(Some(invalid));
            guard.assert_run_is_a_no_op(&format!("{invalid:?}"));
            // Off before the transport: a run with telemetry off never loads
            // OpenSSL, so moving the check ahead of the destination fails this.
            assert_eq!(TRANSPORT_CHECKS.load(Ordering::Acquire), 0, "{invalid:?}");
        }
    }

    /// A host that cannot send (no OpenSSL) does nothing: no device ID, no server
    /// query, no event. Without `&& transport_available()` in `enabled()` this
    /// fails.
    #[test]
    fn a_host_that_cannot_send_is_a_no_op_with_no_side_effects() {
        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        TRANSPORT_MISSING.store(true, Ordering::Release);
        guard.assert_run_is_a_no_op("no transport");
        // The destination is valid here, so the transport is what was asked.
        assert!(TRANSPORT_CHECKS.load(Ordering::Acquire) > 0);
    }

    #[test]
    fn a_valid_connection_string_is_enabled() {
        let _guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        assert!(enabled());
    }

    /// The service's answer decides the outcome the C ABI reports, and the event
    /// is sent once whatever the answer: there is no retry.
    #[test]
    fn the_answer_decides_the_outcome_and_nothing_is_retried() {
        use FlushOutcome::{Completed, Failed};
        for (status, body, expected) in [
            (200, "", Completed),
            (
                206,
                r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#,
                Completed,
            ),
            (
                206,
                r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[{"index":0,"statusCode":429}]}"#,
                Failed,
            ),
            (
                206,
                r#"{"itemsReceived":1,"itemsAccepted":0,"errors":[]}"#,
                Failed,
            ),
            (206, "not json", Failed),
            (204, "", Failed),
            (400, "", Failed),
            (403, "", Failed),
            (429, "", Failed),
            (500, "", Failed),
            (503, "", Failed),
            // A transport error.
            (0, "", Failed),
        ] {
            let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
            let requests = guard.capture_requests();
            guard.respond_with(status, body);
            record_usage(Invocation {
                version: Some("18.5"),
                command: Command::Unspecified,
                format: FormatUse::Absent,
            });

            let outcome = shutdown(Duration::from_secs(1));

            assert_eq!(outcome, expected, "{status} {body}");
            assert!(requests.try_recv().is_ok(), "the event was sent: {status}");
            // Give a retry, if there were one, time to arrive.
            assert!(
                requests.recv_timeout(Duration::from_millis(100)).is_err(),
                "no retry: {status}"
            );
        }
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

    /// A send slower than the cap is cut off at the cap, whatever the caller
    /// asks for: the library's cap, not the request, bounds the delay at exit.
    /// Both the cap itself and a much longer request give `TimedOut` in about
    /// half a second.
    #[test]
    fn shutdown_waits_no_longer_than_the_cap_whatever_is_asked() {
        assert_eq!(MAX_FLUSH_TIMEOUT, Duration::from_millis(500));
        for requested in [MAX_FLUSH_TIMEOUT, Duration::from_secs(60)] {
            let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
            let _requests = guard.capture_requests();
            *ABI_TEST_DELAY.lock().expect("delay lock") = Duration::from_secs(3);
            record_usage(Invocation {
                version: Some("18.5"),
                command: Command::Unspecified,
                format: FormatUse::Absent,
            });
            let started = std::time::Instant::now();

            let outcome = shutdown(requested);
            *ABI_TEST_DELAY.lock().expect("delay lock") = Duration::ZERO;

            assert_eq!(outcome, FlushOutcome::TimedOut, "{requested:?}");
            assert!(
                started.elapsed() >= Duration::from_millis(400),
                "{requested:?}"
            );
            assert!(
                started.elapsed() < Duration::from_millis(1_200),
                "{requested:?}"
            );
        }
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

    /// The last check before anything leaves the process: a list with a value or
    /// a key outside the allowlist, or without the properties every event has, is
    /// not serialized at all.
    #[test]
    fn an_event_with_unexpected_properties_is_not_built() {
        let destination = Destination::parse(TEST_CONNECTION_STRING).expect("destination");
        let time = SystemTime::UNIX_EPOCH;
        let valid = || {
            vec![
                (KEY_OS, "windows".to_owned()),
                (KEY_ARCH, "x86_64".to_owned()),
                (KEY_VERSION, "18.5".to_owned()),
            ]
        };
        assert!(usage_body(&destination, &valid(), time).is_some());

        let mut bad_value = valid();
        bad_value.push((KEY_COMMAND, "SELECT secret FROM users".to_owned()));
        assert!(usage_body(&destination, &bad_value, time).is_none());

        let mut unknown_key = valid();
        unknown_key.push(("sqlcmdcli_server_name", "prod-db-01".to_owned()));
        assert!(usage_body(&destination, &unknown_key, time).is_none());

        for missing in [KEY_OS, KEY_ARCH] {
            let without: Vec<_> = valid()
                .into_iter()
                .filter(|(key, _)| *key != missing)
                .collect();
            assert!(
                usage_body(&destination, &without, time).is_none(),
                "{missing}"
            );
        }
    }

    /// What goes on the wire is the typed list and nothing the environment can
    /// add: no tags, one item, the configured instrumentation key.
    #[test]
    fn the_wire_event_carries_only_the_properties() {
        let guard = TestEnv::new(Some(TEST_CONNECTION_STRING));
        let requests = guard.capture_requests();
        record_usage(Invocation {
            version: Some("18.5"),
            command: Command::Diagnose,
            format: FormatUse::Json,
        });
        assert_eq!(shutdown(Duration::from_secs(1)), FlushOutcome::Completed);

        let request = requests.try_recv().expect("one request");
        let items: Vec<JsonValue> = serde_json::from_slice(&request.body).expect("JSON array");
        assert_eq!(items.len(), 1);
        let item = &items[0];
        let keys: std::collections::BTreeSet<_> = item
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["data", "iKey", "name", "time"].into_iter().collect(),
            "no tags, no sample rate"
        );
        assert_eq!(item["name"], "Microsoft.ApplicationInsights.Event");
        assert_eq!(item["iKey"], "00000000-0000-0000-0000-000000000000");
        assert_eq!(item["data"]["baseType"], "EventData");
        assert_eq!(item["data"]["baseData"]["ver"], 2);
        assert_eq!(item["data"]["baseData"]["name"], USAGE_EVENT_NAME);
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
                // A declared caller type, so that what the tests assert about the
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
            TRANSPORT_MISSING.store(false, Ordering::Release);
            TRANSPORT_CHECKS.store(0, Ordering::Release);
            *ABI_TEST_DELAY.lock().expect("delay lock") = Duration::ZERO;
            *ABI_TEST_RESPONSE.lock().expect("response lock") = (200, "");
            Self { _lock: lock }
        }

        /// Sends what would be posted to the returned channel instead.
        fn capture_requests(&self) -> mpsc::Receiver<Captured> {
            let (sender, receiver) = mpsc::channel();
            *ABI_TEST_CLIENT.lock().expect("ABI test client lock") = Some(sender);
            receiver
        }

        /// What the service answers with, for the next requests.
        fn respond_with(&self, status: u16, body: &'static str) {
            *ABI_TEST_RESPONSE.lock().expect("response lock") = (status, body);
        }

        /// A run with telemetry off records nothing, reads no device ID and
        /// sends nothing, so shutdown has nothing to wait for. One definition of
        /// that contract: a side effect added to it later is asserted here once.
        fn assert_run_is_a_no_op(&self, case: &str) {
            let requests = self.capture_requests();
            assert!(!enabled(), "{case}");
            record_usage(Invocation {
                version: Some("18.5"),
                command: Command::Diagnose,
                format: FormatUse::Json,
            });
            record_server_info(&server_info_fixture().parsed);
            assert!(SESSION.lock().expect("session lock").is_none(), "{case}");
            assert_eq!(DEVICE_ID_READS.load(Ordering::Acquire), 0, "{case}");
            assert_eq!(
                shutdown(Duration::from_secs(1)),
                FlushOutcome::Completed,
                "{case}"
            );
            assert!(requests.try_recv().is_err(), "{case}");
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

    fn event_properties(request: &Captured) -> std::collections::BTreeMap<String, String> {
        let items: Vec<JsonValue> = serde_json::from_slice(&request.body).expect("request JSON");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]
                .pointer("/data/baseData/name")
                .and_then(JsonValue::as_str),
            Some(USAGE_EVENT_NAME)
        );
        items[0]
            .pointer("/data/baseData/properties")
            .and_then(JsonValue::as_object)
            .expect("event properties")
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().expect("string").to_owned()))
            .collect()
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
}

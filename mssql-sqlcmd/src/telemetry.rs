// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Privacy-safe, exporter-independent product telemetry for native sqlcmd.
//!
//! OpenTelemetry has no first-class custom-event signal. This module preserves
//! one discrete typed usage record behind a sink boundary so an approved
//! provider can map it to Application Insights `AppEvents`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, SystemTime};

pub const USAGE_EVENT_NAME: &str = "sqlcmdcli_usage";
pub const MAX_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

const SCHEMA_VERSION: &str = "1";
const IMPLEMENTATION: &str = "mssql-sqlcmd-rust";
const RUNTIME: &str = "native";
const MAX_VERSION_LEN: usize = 64;

static EVENT_SINK: OnceLock<Box<dyn EventSink>> = OnceLock::new();
static SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);

/// One discrete product usage event ready for an approved provider to export.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageEvent {
    common: CommonProperties,
}

impl UsageEvent {
    pub fn name(&self) -> &'static str {
        USAGE_EVENT_NAME
    }

    pub fn common(&self) -> &CommonProperties {
        &self.common
    }
}

/// Common bounded properties for the sqlcmd usage event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommonProperties {
    timestamp: SystemTime,
    schema_version: &'static str,
    sqlcmd_version: Option<String>,
    os: &'static str,
    architecture: &'static str,
    implementation: &'static str,
    runtime: &'static str,
}

impl CommonProperties {
    pub fn timestamp(&self) -> SystemTime {
        self.timestamp
    }

    pub fn schema_version(&self) -> &'static str {
        self.schema_version
    }

    pub fn sqlcmd_version(&self) -> Option<&str> {
        self.sqlcmd_version.as_deref()
    }

    pub fn os(&self) -> &'static str {
        self.os
    }

    pub fn architecture(&self) -> &'static str {
        self.architecture
    }

    pub fn implementation(&self) -> &'static str {
        self.implementation
    }

    pub fn runtime(&self) -> &'static str {
        self.runtime
    }
}

/// Provider boundary implemented by a future approved Application Insights
/// exporter. Implementations must handle their own failures without panicking.
pub trait EventSink: Send + Sync {
    fn record(&self, event: &UsageEvent);

    fn flush(&self, _timeout: Duration) -> Result<(), FlushError> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventSinkAlreadyInstalled;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushOutcome {
    Completed,
    Failed,
    TimedOut,
}

/// Installs the process-wide event sink before native sqlcmd records usage.
///
/// The first provider wins so event routing cannot change during a process.
pub fn install_event_sink(sink: impl EventSink + 'static) -> Result<(), EventSinkAlreadyInstalled> {
    EVENT_SINK
        .set(Box::new(sink))
        .map_err(|_| EventSinkAlreadyInstalled)
}

/// Captures one sqlcmd usage event. With no installed provider this is a no-op.
pub fn record_usage(sqlcmd_version: Option<&str>) {
    record_usage_at(sqlcmd_version, SystemTime::now(), installed_sink());
}

/// Stops new capture and flushes the installed provider once.
///
/// The timeout is clamped to [`MAX_FLUSH_TIMEOUT`]. With no provider, or after
/// another shutdown call has started, this returns [`FlushOutcome::Completed`].
pub fn shutdown(timeout: Duration) -> FlushOutcome {
    if SHUTDOWN_STARTED.swap(true, Ordering::AcqRel) {
        return FlushOutcome::Completed;
    }
    let Some(sink) = installed_sink() else {
        return FlushOutcome::Completed;
    };
    let timeout = clamp_flush_timeout(timeout);
    flush_with_timeout(sink, timeout)
}

fn clamp_flush_timeout(timeout: Duration) -> Duration {
    timeout.min(MAX_FLUSH_TIMEOUT)
}

fn flush_with_timeout(sink: &'static dyn EventSink, timeout: Duration) -> FlushOutcome {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.flush(timeout))).map_or(
                FlushOutcome::Failed,
                |result| match result {
                    Ok(()) => FlushOutcome::Completed,
                    Err(_) => FlushOutcome::Failed,
                },
            );
        let _ = sender.send(outcome);
    });
    match receiver.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(mpsc::RecvTimeoutError::Timeout) => FlushOutcome::TimedOut,
        Err(mpsc::RecvTimeoutError::Disconnected) => FlushOutcome::Failed,
    }
}

fn installed_sink() -> Option<&'static dyn EventSink> {
    EVENT_SINK.get().map(Box::as_ref)
}

fn record_usage_at(
    sqlcmd_version: Option<&str>,
    timestamp: SystemTime,
    sink: Option<&dyn EventSink>,
) {
    if !SHUTDOWN_STARTED.load(Ordering::Acquire)
        && let Some(sink) = sink
    {
        sink.record(&UsageEvent {
            common: common_properties(sqlcmd_version, timestamp),
        });
    }
}

fn common_properties(sqlcmd_version: Option<&str>, timestamp: SystemTime) -> CommonProperties {
    CommonProperties {
        timestamp,
        schema_version: SCHEMA_VERSION,
        sqlcmd_version: sqlcmd_version
            .filter(|version| version_is_safe(version))
            .map(ToOwned::to_owned),
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        implementation: IMPLEMENTATION,
        runtime: RUNTIME,
    }
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
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    #[derive(Default)]
    struct InMemorySink {
        events: Mutex<Vec<UsageEvent>>,
    }

    impl EventSink for InMemorySink {
        fn record(&self, event: &UsageEvent) {
            self.events
                .lock()
                .expect("in-memory telemetry lock")
                .push(event.clone());
        }
    }

    #[test]
    fn usage_is_captured_as_an_exact_discrete_record() {
        let timestamp = UNIX_EPOCH + Duration::from_secs(1_730_736_000);
        let sink = InMemorySink::default();

        record_usage_at(Some("18.4.1-preview.2"), timestamp, Some(&sink));

        assert_eq!(
            *sink.events.lock().expect("in-memory telemetry lock"),
            vec![UsageEvent {
                common: CommonProperties {
                    timestamp,
                    schema_version: "1",
                    sqlcmd_version: Some("18.4.1-preview.2".to_owned()),
                    os: std::env::consts::OS,
                    architecture: std::env::consts::ARCH,
                    implementation: "mssql-sqlcmd-rust",
                    runtime: "native",
                },
            }]
        );
    }

    #[test]
    fn unsafe_or_free_form_versions_are_not_recorded() {
        for version in [
            "",
            "18.4 user@example.com",
            "18.4/server",
            "18.4\\path",
            "x".repeat(MAX_VERSION_LEN + 1).as_str(),
        ] {
            let properties = common_properties(Some(version), UNIX_EPOCH);
            assert_eq!(properties.sqlcmd_version, None);
        }
    }

    #[test]
    fn no_provider_is_a_no_op() {
        record_usage_at(Some("18.4.1"), UNIX_EPOCH, None);
    }

    #[test]
    fn event_name_is_stable() {
        let usage = UsageEvent {
            common: common_properties(None, UNIX_EPOCH),
        };
        assert_eq!(usage.name(), USAGE_EVENT_NAME);
    }

    struct SlowSink;

    impl EventSink for SlowSink {
        fn record(&self, _event: &UsageEvent) {}

        fn flush(&self, _timeout: Duration) -> Result<(), FlushError> {
            std::thread::sleep(Duration::from_millis(100));
            Ok(())
        }
    }

    #[test]
    fn flush_wait_is_bounded_independently_of_the_provider() {
        static SINK: SlowSink = SlowSink;
        assert_eq!(
            flush_with_timeout(&SINK, Duration::from_millis(1)),
            FlushOutcome::TimedOut
        );
    }

    #[test]
    fn flush_timeout_is_clamped_to_one_second() {
        assert_eq!(
            clamp_flush_timeout(Duration::from_secs(30)),
            MAX_FLUSH_TIMEOUT
        );
    }

    struct PanickingSink;

    impl EventSink for PanickingSink {
        fn record(&self, _event: &UsageEvent) {}

        fn flush(&self, _timeout: Duration) -> Result<(), FlushError> {
            panic!("provider panic")
        }
    }

    #[test]
    fn provider_panic_is_contained() {
        static SINK: PanickingSink = PanickingSink;
        assert_eq!(
            flush_with_timeout(&SINK, Duration::from_secs(1)),
            FlushOutcome::Failed
        );
    }
}

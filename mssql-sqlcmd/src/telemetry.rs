// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Privacy-safe, exporter-independent product telemetry for native sqlcmd.
//!
//! OpenTelemetry has no first-class custom-event signal. This module preserves
//! discrete typed event records behind a sink boundary so an approved provider
//! can map them to Application Insights `AppEvents` without changing capture.

use std::sync::OnceLock;
use std::time::SystemTime;

pub const INVOCATION_EVENT_NAME: &str = "sqlcmdcli_invocation";
pub const COMPLETION_EVENT_NAME: &str = "sqlcmdcli_completion";

const SCHEMA_VERSION: &str = "1";
const IMPLEMENTATION: &str = "mssql-sqlcmd-rust";
const RUNTIME: &str = "native";
const MAX_VERSION_LEN: usize = 64;

static EVENT_SINK: OnceLock<Box<dyn EventSink>> = OnceLock::new();

/// A discrete product event ready for an approved provider to export.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelemetryEvent {
    Invocation(InvocationEvent),
    Completion(CompletionEvent),
}

impl TelemetryEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Invocation(_) => INVOCATION_EVENT_NAME,
            Self::Completion(_) => COMPLETION_EVENT_NAME,
        }
    }

    pub fn common(&self) -> &CommonProperties {
        match self {
            Self::Invocation(event) => &event.common,
            Self::Completion(event) => &event.common,
        }
    }
}

/// Common bounded properties shared by every sqlcmd product event.
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

/// One native sqlcmd process invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationEvent {
    common: CommonProperties,
}

/// One completed native sqlcmd process invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionEvent {
    common: CommonProperties,
    outcome: CompletionOutcome,
}

impl CompletionEvent {
    pub fn outcome(&self) -> CompletionOutcome {
        self.outcome
    }
}

/// Bounded outcome categories for a completed sqlcmd invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionOutcome {
    Succeeded,
    Failed,
    Canceled,
    InvalidInvocation,
    Unknown,
}

impl CompletionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::InvalidInvocation => "invalid_invocation",
            Self::Unknown => "unknown",
        }
    }
}

/// Provider boundary implemented by a future approved Application Insights
/// exporter. Implementations must handle their own failures without panicking.
pub trait EventSink: Send + Sync {
    fn record(&self, event: &TelemetryEvent);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventSinkAlreadyInstalled;

/// Installs the process-wide event sink before native sqlcmd records events.
///
/// The first provider wins so event routing cannot change during a process.
pub fn install_event_sink(sink: impl EventSink + 'static) -> Result<(), EventSinkAlreadyInstalled> {
    EVENT_SINK
        .set(Box::new(sink))
        .map_err(|_| EventSinkAlreadyInstalled)
}

/// Captures one process invocation. With no installed provider this is a no-op.
pub fn record_invocation(sqlcmd_version: Option<&str>) {
    record_invocation_at(sqlcmd_version, SystemTime::now(), installed_sink());
}

/// Captures one process completion. With no installed provider this is a no-op.
pub fn record_completion(sqlcmd_version: Option<&str>, outcome: CompletionOutcome) {
    record_completion_at(sqlcmd_version, outcome, SystemTime::now(), installed_sink());
}

fn installed_sink() -> Option<&'static dyn EventSink> {
    EVENT_SINK.get().map(Box::as_ref)
}

fn record_invocation_at(
    sqlcmd_version: Option<&str>,
    timestamp: SystemTime,
    sink: Option<&dyn EventSink>,
) {
    if let Some(sink) = sink {
        sink.record(&TelemetryEvent::Invocation(InvocationEvent {
            common: common_properties(sqlcmd_version, timestamp),
        }));
    }
}

fn record_completion_at(
    sqlcmd_version: Option<&str>,
    outcome: CompletionOutcome,
    timestamp: SystemTime,
    sink: Option<&dyn EventSink>,
) {
    if let Some(sink) = sink {
        sink.record(&TelemetryEvent::Completion(CompletionEvent {
            common: common_properties(sqlcmd_version, timestamp),
            outcome,
        }));
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
        events: Mutex<Vec<TelemetryEvent>>,
    }

    impl EventSink for InMemorySink {
        fn record(&self, event: &TelemetryEvent) {
            self.events
                .lock()
                .expect("in-memory telemetry lock")
                .push(event.clone());
        }
    }

    #[test]
    fn invocation_is_captured_as_an_exact_discrete_record() {
        let timestamp = UNIX_EPOCH + Duration::from_secs(1_730_736_000);
        let sink = InMemorySink::default();

        record_invocation_at(Some("18.4.1-preview.2"), timestamp, Some(&sink));

        assert_eq!(
            *sink.events.lock().expect("in-memory telemetry lock"),
            vec![TelemetryEvent::Invocation(InvocationEvent {
                common: CommonProperties {
                    timestamp,
                    schema_version: "1",
                    sqlcmd_version: Some("18.4.1-preview.2".to_owned()),
                    os: std::env::consts::OS,
                    architecture: std::env::consts::ARCH,
                    implementation: "mssql-sqlcmd-rust",
                    runtime: "native",
                },
            })]
        );
    }

    #[test]
    fn completion_is_captured_as_an_exact_discrete_record() {
        let timestamp = UNIX_EPOCH + Duration::from_secs(1_730_736_001);
        let sink = InMemorySink::default();

        record_completion_at(
            Some("18.4.1"),
            CompletionOutcome::Canceled,
            timestamp,
            Some(&sink),
        );

        let events = sink.events.lock().expect("in-memory telemetry lock");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name(), COMPLETION_EVENT_NAME);
        assert_eq!(events[0].common().timestamp, timestamp);
        assert_eq!(
            events[0],
            TelemetryEvent::Completion(CompletionEvent {
                common: CommonProperties {
                    timestamp,
                    schema_version: "1",
                    sqlcmd_version: Some("18.4.1".to_owned()),
                    os: std::env::consts::OS,
                    architecture: std::env::consts::ARCH,
                    implementation: "mssql-sqlcmd-rust",
                    runtime: "native",
                },
                outcome: CompletionOutcome::Canceled,
            })
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
        record_invocation_at(Some("18.4.1"), UNIX_EPOCH, None);
        record_completion_at(
            Some("18.4.1"),
            CompletionOutcome::Succeeded,
            UNIX_EPOCH,
            None,
        );
    }

    #[test]
    fn event_names_and_outcomes_are_bounded() {
        let invocation = TelemetryEvent::Invocation(InvocationEvent {
            common: common_properties(None, UNIX_EPOCH),
        });
        assert_eq!(invocation.name(), INVOCATION_EVENT_NAME);
        assert_eq!(CompletionOutcome::Succeeded.as_str(), "succeeded");
        assert_eq!(CompletionOutcome::Failed.as_str(), "failed");
        assert_eq!(CompletionOutcome::Canceled.as_str(), "canceled");
        assert_eq!(
            CompletionOutcome::InvalidInvocation.as_str(),
            "invalid_invocation"
        );
        assert_eq!(CompletionOutcome::Unknown.as_str(), "unknown");
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Privacy-safe, exporter-independent product telemetry for native sqlcmd.
//!
//! The global OpenTelemetry meter is a no-op unless the host installs a meter
//! provider. This crate deliberately does not choose or configure an exporter.

use opentelemetry::metrics::{Counter, Meter};
use opentelemetry::{KeyValue, global};
use std::sync::OnceLock;

const INSTRUMENTATION_SCOPE: &str = "mssql-sqlcmd";
const SCHEMA_VERSION: &str = "1";
const IMPLEMENTATION: &str = "mssql-sqlcmd-rust";
const RUNTIME: &str = "native";
const MAX_VERSION_LEN: usize = 64;

const SCHEMA_VERSION_KEY: &str = "sqlcmdcli_schema_version";
const SQLCMD_VERSION_KEY: &str = "sqlcmdcli_version";
const OS_KEY: &str = "sqlcmdcli_os";
const ARCH_KEY: &str = "sqlcmdcli_arch";
const IMPLEMENTATION_KEY: &str = "sqlcmdcli_implementation";
const RUNTIME_KEY: &str = "sqlcmdcli_runtime";
const OUTCOME_KEY: &str = "sqlcmdcli_outcome";

const INVOCATION_METRIC: &str = "sqlcmdcli_invocation";
const COMPLETION_METRIC: &str = "sqlcmdcli_completion";

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

#[derive(Debug)]
struct Instruments {
    invocation: Counter<u64>,
    completion: Counter<u64>,
}

impl Instruments {
    fn new(meter: &Meter) -> Self {
        Self {
            invocation: meter
                .u64_counter(INVOCATION_METRIC)
                .with_description("Number of sqlcmd invocations")
                .build(),
            completion: meter
                .u64_counter(COMPLETION_METRIC)
                .with_description("Number of completed sqlcmd invocations")
                .build(),
        }
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
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::InvalidInvocation => "invalid_invocation",
            Self::Unknown => "unknown",
        }
    }
}

/// Records one process invocation using only bounded common properties.
pub fn record_invocation(sqlcmd_version: Option<&str>) {
    instruments()
        .invocation
        .add(1, &common_attributes(sqlcmd_version));
}

/// Records one process completion without the exit code or error details.
pub fn record_completion(sqlcmd_version: Option<&str>, outcome: CompletionOutcome) {
    let mut attributes = common_attributes(sqlcmd_version);
    attributes.push(KeyValue::new(OUTCOME_KEY, outcome.as_str()));
    instruments().completion.add(1, &attributes);
}

fn instruments() -> &'static Instruments {
    INSTRUMENTS.get_or_init(|| Instruments::new(&global::meter(INSTRUMENTATION_SCOPE)))
}

fn common_attributes(sqlcmd_version: Option<&str>) -> Vec<KeyValue> {
    let mut attributes = Vec::with_capacity(6);
    attributes.push(KeyValue::new(SCHEMA_VERSION_KEY, SCHEMA_VERSION));
    attributes.push(KeyValue::new(OS_KEY, std::env::consts::OS));
    attributes.push(KeyValue::new(ARCH_KEY, std::env::consts::ARCH));
    attributes.push(KeyValue::new(IMPLEMENTATION_KEY, IMPLEMENTATION));
    attributes.push(KeyValue::new(RUNTIME_KEY, RUNTIME));
    if let Some(version) = sqlcmd_version.filter(|version| version_is_safe(version)) {
        attributes.push(KeyValue::new(SQLCMD_VERSION_KEY, version.to_owned()));
    }
    attributes
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
    use opentelemetry::Value;

    fn attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a Value> {
        attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| &attribute.value)
    }

    #[test]
    fn common_properties_have_stable_bounded_values() {
        let attributes = common_attributes(Some("18.4.1-preview.2"));

        assert_eq!(
            attribute(&attributes, SCHEMA_VERSION_KEY),
            Some(&Value::from(SCHEMA_VERSION))
        );
        assert_eq!(
            attribute(&attributes, SQLCMD_VERSION_KEY),
            Some(&Value::from("18.4.1-preview.2"))
        );
        assert_eq!(
            attribute(&attributes, OS_KEY),
            Some(&Value::from(std::env::consts::OS))
        );
        assert_eq!(
            attribute(&attributes, ARCH_KEY),
            Some(&Value::from(std::env::consts::ARCH))
        );
        assert_eq!(
            attribute(&attributes, IMPLEMENTATION_KEY),
            Some(&Value::from(IMPLEMENTATION))
        );
        assert_eq!(
            attribute(&attributes, RUNTIME_KEY),
            Some(&Value::from(RUNTIME))
        );
        assert_eq!(attributes.len(), 6);
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
            let attributes = common_attributes(Some(version));
            assert_eq!(attribute(&attributes, SQLCMD_VERSION_KEY), None);
            assert_eq!(attributes.len(), 5);
        }
    }

    #[test]
    fn completion_outcomes_are_bounded() {
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

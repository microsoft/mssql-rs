// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Who is calling sqlcmd, for usage telemetry.
//!
//! A caller can declare what it is with [`DECLARED_CALLER_ENV`]. Without a
//! declaration, AI agent detection uses the environment markers DacFx and
//! SqlPackage use (`TelemetryUtils.GetAiAgentSource`), so a run one of those
//! tools reports as an agent is an agent here too. Only the bounded category is
//! reported: the marker that matched, raw marker values, session identifiers and
//! prompts are never read into an event.
//!
//! No marker does not prove a human: sqlcmd cannot tell, so it reports
//! `unknown`, and `human` only when the caller declares it.

/// The kind of caller: what it declared, else what the markers sqlcmd can see
/// show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallerType {
    Human,
    Agent,
    Pipeline,
    OtherAutomation,
    Unknown,
}

impl CallerType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::Pipeline => "pipeline",
            Self::OtherAutomation => "other_automation",
            Self::Unknown => "unknown",
        }
    }

    fn from_declared(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "human" => Some(Self::Human),
            "agent" => Some(Self::Agent),
            "pipeline" => Some(Self::Pipeline),
            "other_automation" => Some(Self::OtherAutomation),
            _ => None,
        }
    }
}

/// The environment variable a caller sets to say what it is: `human`, `agent`,
/// `pipeline` or `other_automation`. It wins over the markers. Any other value
/// is ignored, so a typo falls back to detection.
pub const DECLARED_CALLER_ENV: &str = "SQLCMD_CALLER_TYPE";

/// A marker is set to exactly this value, or to any non-empty value.
enum Marker {
    Equals(&'static str, &'static str),
    NonEmpty(&'static str),
}

/// DacFx's AI agent markers. Any one of them makes the caller an agent, so
/// their order does not matter here.
const AGENT_MARKERS: &[Marker] = &[
    Marker::Equals("AI_AGENT", "github_copilot_vscode_agent"),
    Marker::Equals("CLAUDE_CODE_CHILD_SESSION", "1"),
    Marker::NonEmpty("CODEX_THREAD_ID"),
    Marker::NonEmpty("CODEX_SESSION_ID"),
    Marker::Equals("GEMINI_CLI", "1"),
    Marker::Equals("CLAUDECODE", "1"),
    Marker::Equals("COPILOT_AGENT", "1"),
];

/// CI systems DacFx recognizes by presence. `AWS_REGION` and
/// `AWS_DEFAULT_REGION`, which DacFx also counts for AWS CodeBuild, are left
/// out: they are set on most AWS machines, not only in builds.
const PIPELINE_MARKERS: &[&str] = &[
    "AGENT_JOBNAME",
    "TF_BUILD",
    "SYSTEM_COLLECTIONURI",
    "GITHUB_ACTION",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "JENKINS_URL",
    "CIRCLECI",
    "CODEBUILD_BUILD_ID",
];

/// Detects the caller from the process environment.
pub fn detect() -> CallerType {
    detect_with(|name| std::env::var(name).ok())
}

/// Detects the caller from `get`, which returns an environment variable's
/// value, or `None` when it is not set or not valid text.
pub fn detect_with(get: impl Fn(&str) -> Option<String>) -> CallerType {
    if let Some(declared) = get(DECLARED_CALLER_ENV)
        .as_deref()
        .and_then(CallerType::from_declared)
    {
        return declared;
    }
    let agent = AGENT_MARKERS.iter().any(|marker| match marker {
        Marker::Equals(name, value) => get(name).as_deref() == Some(*value),
        Marker::NonEmpty(name) => get(name).is_some_and(|value| !value.is_empty()),
    });
    if agent {
        return CallerType::Agent;
    }
    let pipeline = PIPELINE_MARKERS
        .iter()
        .any(|name| get(name).is_some_and(|value| !value.is_empty()));
    if pipeline {
        CallerType::Pipeline
    } else {
        CallerType::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn caller(vars: &[(&str, &str)]) -> CallerType {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        detect_with(|name| map.get(name).cloned())
    }

    #[test]
    fn each_agent_marker_is_detected_as_dacfx_does() {
        for (name, value) in [
            ("AI_AGENT", "github_copilot_vscode_agent"),
            ("CLAUDE_CODE_CHILD_SESSION", "1"),
            ("CODEX_THREAD_ID", "abc"),
            ("CODEX_SESSION_ID", "abc"),
            ("GEMINI_CLI", "1"),
            ("CLAUDECODE", "1"),
            ("COPILOT_AGENT", "1"),
        ] {
            assert_eq!(caller(&[(name, value)]), CallerType::Agent, "{name}");
        }
    }

    #[test]
    fn markers_need_their_exact_value() {
        for vars in [
            &[("AI_AGENT", "something_else")][..],
            &[("CLAUDECODE", "true")][..],
            &[("GEMINI_CLI", "0")][..],
            &[("CODEX_THREAD_ID", "")][..],
            &[("COPILOT_AGENT", "yes")][..],
        ] {
            assert_eq!(caller(vars), CallerType::Unknown, "{vars:?}");
        }
    }

    #[test]
    fn a_pipeline_without_an_agent_is_a_pipeline() {
        for name in [
            "TF_BUILD",
            "GITHUB_ACTIONS",
            "GITLAB_CI",
            "JENKINS_URL",
            "CIRCLECI",
        ] {
            assert_eq!(caller(&[(name, "true")]), CallerType::Pipeline, "{name}");
        }
    }

    #[test]
    fn an_agent_inside_a_pipeline_is_an_agent() {
        assert_eq!(
            caller(&[("GITHUB_ACTIONS", "true"), ("CLAUDECODE", "1")]),
            CallerType::Agent
        );
    }

    #[test]
    fn aws_region_alone_is_not_a_pipeline() {
        assert_eq!(
            caller(&[
                ("AWS_REGION", "us-east-1"),
                ("AWS_DEFAULT_REGION", "us-east-1"),
            ]),
            CallerType::Unknown
        );
    }

    #[test]
    fn a_declared_caller_wins_over_markers() {
        assert_eq!(
            caller(&[
                (DECLARED_CALLER_ENV, "human"),
                ("CLAUDECODE", "1"),
                ("GITHUB_ACTIONS", "true"),
            ]),
            CallerType::Human
        );
        for (declared, expected) in [
            ("agent", CallerType::Agent),
            (" Pipeline ", CallerType::Pipeline),
            ("OTHER_AUTOMATION", CallerType::OtherAutomation),
        ] {
            assert_eq!(
                caller(&[(DECLARED_CALLER_ENV, declared)]),
                expected,
                "{declared}"
            );
        }
    }

    #[test]
    fn an_unrecognized_declaration_is_ignored() {
        assert_eq!(
            caller(&[(DECLARED_CALLER_ENV, "robot"), ("CLAUDECODE", "1")]),
            CallerType::Agent
        );
        assert_eq!(caller(&[(DECLARED_CALLER_ENV, "")]), CallerType::Unknown);
    }

    #[test]
    fn nothing_set_is_unknown_not_human() {
        assert_eq!(caller(&[]), CallerType::Unknown);
        assert_eq!(CallerType::Unknown.as_str(), "unknown");
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Who is calling sqlcmd, for usage telemetry.
//!
//! AI agent detection matches DacFx and SqlPackage (`TelemetryUtils.GetAiAgentSource`):
//! the same environment markers, in the same order, with the same result
//! names, so a run one of those tools reports as an agent is an agent here too.
//! Only the bounded category is reported. Raw marker values, session
//! identifiers and prompts are never read into an event.
//!
//! `None` does not prove a human: sqlcmd cannot tell. It means no recognized
//! marker was present.

/// The AI agent environment sqlcmd runs in, named as DacFx names them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AiAgentSource {
    None,
    GitHubCopilotVsCode,
    ClaudeCode,
    OpenAICodex,
    GeminiCli,
}

impl AiAgentSource {
    /// The reported value; the same text DacFx reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::GitHubCopilotVsCode => "GitHubCopilotVsCode",
            Self::ClaudeCode => "ClaudeCode",
            Self::OpenAICodex => "OpenAICodex",
            Self::GeminiCli => "GeminiCli",
        }
    }
}

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
    pub fn as_str(self) -> &'static str {
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

/// What detection found. An `agent` with a source was inferred from a marker;
/// without one it was declared, so the two stay distinguishable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Caller {
    pub caller_type: CallerType,
    pub agent: AiAgentSource,
}

/// A marker is set to exactly this value.
enum Marker {
    Equals(&'static str, &'static str),
    NonEmpty(&'static str),
}

/// DacFx's order. The first marker that matches decides.
const AGENT_MARKERS: &[(Marker, AiAgentSource)] = &[
    (
        Marker::Equals("AI_AGENT", "github_copilot_vscode_agent"),
        AiAgentSource::GitHubCopilotVsCode,
    ),
    (
        Marker::Equals("CLAUDE_CODE_CHILD_SESSION", "1"),
        AiAgentSource::ClaudeCode,
    ),
    (
        Marker::NonEmpty("CODEX_THREAD_ID"),
        AiAgentSource::OpenAICodex,
    ),
    (
        Marker::NonEmpty("CODEX_SESSION_ID"),
        AiAgentSource::OpenAICodex,
    ),
    (Marker::Equals("GEMINI_CLI", "1"), AiAgentSource::GeminiCli),
    (Marker::Equals("CLAUDECODE", "1"), AiAgentSource::ClaudeCode),
    (
        Marker::Equals("COPILOT_AGENT", "1"),
        AiAgentSource::GitHubCopilotVsCode,
    ),
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
pub fn detect() -> Caller {
    detect_with(|name| std::env::var(name).ok())
}

/// Detects the caller from `get`, which returns an environment variable's
/// value, or `None` when it is not set or not valid text.
pub fn detect_with(get: impl Fn(&str) -> Option<String>) -> Caller {
    if let Some(declared) = get(DECLARED_CALLER_ENV)
        .as_deref()
        .and_then(CallerType::from_declared)
    {
        return Caller {
            caller_type: declared,
            agent: AiAgentSource::None,
        };
    }
    for (marker, source) in AGENT_MARKERS {
        let hit = match marker {
            Marker::Equals(name, value) => get(name).as_deref() == Some(*value),
            Marker::NonEmpty(name) => get(name).is_some_and(|value| !value.is_empty()),
        };
        if hit {
            return Caller {
                caller_type: CallerType::Agent,
                agent: *source,
            };
        }
    }
    let pipeline = PIPELINE_MARKERS
        .iter()
        .any(|name| get(name).is_some_and(|value| !value.is_empty()));
    Caller {
        caller_type: if pipeline {
            CallerType::Pipeline
        } else {
            CallerType::Unknown
        },
        agent: AiAgentSource::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn caller(vars: &[(&str, &str)]) -> Caller {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        detect_with(|name| map.get(name).cloned())
    }

    #[test]
    fn each_agent_marker_is_detected_as_dacfx_does() {
        let cases = [
            (
                ("AI_AGENT", "github_copilot_vscode_agent"),
                AiAgentSource::GitHubCopilotVsCode,
            ),
            (
                ("CLAUDE_CODE_CHILD_SESSION", "1"),
                AiAgentSource::ClaudeCode,
            ),
            (("CODEX_THREAD_ID", "abc"), AiAgentSource::OpenAICodex),
            (("CODEX_SESSION_ID", "abc"), AiAgentSource::OpenAICodex),
            (("GEMINI_CLI", "1"), AiAgentSource::GeminiCli),
            (("CLAUDECODE", "1"), AiAgentSource::ClaudeCode),
            (("COPILOT_AGENT", "1"), AiAgentSource::GitHubCopilotVsCode),
        ];
        for ((name, value), expected) in cases {
            let found = caller(&[(name, value)]);
            assert_eq!(found.agent, expected, "{name}");
            assert_eq!(found.caller_type, CallerType::Agent, "{name}");
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
            let found = caller(vars);
            assert_eq!(found.agent, AiAgentSource::None, "{vars:?}");
            assert_eq!(found.caller_type, CallerType::Unknown, "{vars:?}");
        }
    }

    #[test]
    fn dacfx_precedence_is_kept_when_several_markers_are_present() {
        let found = caller(&[
            ("GEMINI_CLI", "1"),
            ("AI_AGENT", "github_copilot_vscode_agent"),
        ]);
        assert_eq!(found.agent, AiAgentSource::GitHubCopilotVsCode);
        let found = caller(&[("CLAUDECODE", "1"), ("CODEX_THREAD_ID", "x")]);
        assert_eq!(found.agent, AiAgentSource::OpenAICodex);
        let found = caller(&[("COPILOT_AGENT", "1"), ("GEMINI_CLI", "1")]);
        assert_eq!(found.agent, AiAgentSource::GeminiCli);
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
            let found = caller(&[(name, "true")]);
            assert_eq!(found.caller_type, CallerType::Pipeline, "{name}");
            assert_eq!(found.agent, AiAgentSource::None, "{name}");
        }
    }

    #[test]
    fn an_agent_inside_a_pipeline_is_an_agent() {
        let found = caller(&[("GITHUB_ACTIONS", "true"), ("CLAUDECODE", "1")]);
        assert_eq!(found.caller_type, CallerType::Agent);
        assert_eq!(found.agent, AiAgentSource::ClaudeCode);
    }

    #[test]
    fn aws_region_alone_is_not_a_pipeline() {
        let found = caller(&[
            ("AWS_REGION", "us-east-1"),
            ("AWS_DEFAULT_REGION", "us-east-1"),
        ]);
        assert_eq!(found.caller_type, CallerType::Unknown);
    }

    #[test]
    fn a_declared_caller_wins_over_markers() {
        let found = caller(&[
            (DECLARED_CALLER_ENV, "human"),
            ("CLAUDECODE", "1"),
            ("GITHUB_ACTIONS", "true"),
        ]);
        assert_eq!(found.caller_type, CallerType::Human);
        assert_eq!(found.agent, AiAgentSource::None);
        for (declared, expected) in [
            ("agent", CallerType::Agent),
            (" Pipeline ", CallerType::Pipeline),
            ("OTHER_AUTOMATION", CallerType::OtherAutomation),
        ] {
            let found = caller(&[(DECLARED_CALLER_ENV, declared)]);
            assert_eq!(found.caller_type, expected, "{declared}");
            assert_eq!(found.agent, AiAgentSource::None, "{declared}");
        }
    }

    #[test]
    fn a_declared_agent_has_no_source_so_it_differs_from_an_inferred_one() {
        let declared = caller(&[(DECLARED_CALLER_ENV, "agent")]);
        let inferred = caller(&[("CLAUDECODE", "1")]);
        assert_eq!(declared.caller_type, inferred.caller_type);
        assert_eq!(declared.agent, AiAgentSource::None);
        assert_eq!(inferred.agent, AiAgentSource::ClaudeCode);
    }

    #[test]
    fn an_unrecognized_declaration_is_ignored() {
        let found = caller(&[(DECLARED_CALLER_ENV, "robot"), ("CLAUDECODE", "1")]);
        assert_eq!(found.caller_type, CallerType::Agent);
        assert_eq!(found.agent, AiAgentSource::ClaudeCode);
        let found = caller(&[(DECLARED_CALLER_ENV, "")]);
        assert_eq!(found.caller_type, CallerType::Unknown);
    }

    #[test]
    fn nothing_set_is_unknown_not_human() {
        let found = caller(&[]);
        assert_eq!(found.caller_type, CallerType::Unknown);
        assert_eq!(found.agent, AiAgentSource::None);
        assert_eq!(CallerType::Unknown.as_str(), "unknown");
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Renders a diagnose [`Report`] as text, for people, or as one JSON document,
//! for scripts and tools. Both carry the same information: the requested depth,
//! the deepest phase evaluated, the execution status, the diagnostic outcome
//! and what its exit code means, every check (with why it was skipped), the
//! findings, the specialist domain, the error chain and the limitations.
//!
//! Output is share-safe by default: server, instance, database, host, address
//! and user identifiers are replaced with labels (see [`super::redact`]). With
//! local detail requested they are shown, and the output says it is not
//! share-safe.
//!
//! ```text
//! sqlcmd diagnose: server-1
//! Depth: Connection input; SqlPassword login user-1; encrypt mandatory; server certificate trusted (-C)
//! Share-safe: identifiers are shown as labels (host-1, ...); --local-detail shows them.
//!
//!   Check                 Result          Time  Details
//!   Connection input      PASSED         <1 ms  tcp, host host-1, port 1433
//!
//! Outcome:   passed (exit code 0: every check at the requested depth passed)
//! Execution: completed; deepest phase evaluated: Connection input
//! ```

use std::fmt::Write;

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use super::redact::{Category, Redactor};
use super::{Check, Coverage, Depth, Report, SettingOrigin, SettingSources, Skip};
use crate::formatter::json::{PLATFORM, to_json_string, utc_timestamp};

/// Version of the JSON report's contract, `major.minor`.
pub const CONTRACT_VERSION: &str = "1.0";

/// The operating system family.
fn os_family() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

/// The operating system's name and version, when it can be read.
#[cfg(windows)]
fn os_version() -> Option<String> {
    use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
    // SAFETY: OSVERSIONINFOW is plain data; RtlGetVersion fills the structure
    // whose size field is set, and does not keep the pointer.
    let info = unsafe {
        let mut info: OSVERSIONINFOW = std::mem::zeroed();
        info.dwOSVersionInfoSize = u32::try_from(std::mem::size_of::<OSVERSIONINFOW>()).ok()?;
        (RtlGetVersion(&mut info) == 0).then_some(info)?
    };
    Some(format!(
        "Windows {}.{}.{}",
        info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
    ))
}

#[cfg(target_os = "linux")]
fn os_version() -> Option<String> {
    let release = std::fs::read_to_string("/etc/os-release").ok();
    let name = release.as_deref().and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
            .map(|value| value.trim().trim_matches('"').to_string())
    });
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|k| k.trim().to_string());
    match (name, kernel) {
        (Some(name), Some(kernel)) => Some(format!("{name}; kernel {kernel}")),
        (Some(name), None) => Some(name),
        (None, Some(kernel)) => Some(format!("Linux kernel {kernel}")),
        (None, None) => None,
    }
}

#[cfg(target_os = "macos")]
fn os_version() -> Option<String> {
    let mut buffer = [0u8; 64];
    let mut length = buffer.len();
    // SAFETY: the name is NUL-terminated, and `length` is the buffer's size.
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    let version = std::ffi::CStr::from_bytes_until_nul(&buffer[..length.min(buffer.len())])
        .ok()?
        .to_str()
        .ok()?;
    Some(format!("macOS {version}"))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn os_version() -> Option<String> {
    None
}

/// A redactor that has seen every identifier of the report, in a fixed order,
/// so text and JSON give each value the same label.
fn redactor(report: &Report) -> Redactor {
    let mut r = Redactor::new(report.request.local_detail);
    r.name(Category::Server, &report.request.server);
    if let Some(target) = &report.target {
        r.name(Category::Host, &target.host);
        if let Some(instance) = &target.instance {
            r.name(Category::Instance, instance);
        }
    }
    if let Some(database) = &report.request.database {
        r.name(Category::Database, database);
    }
    if let Some(name) = &report.request.host_name_in_certificate {
        r.name(Category::Host, name);
    }
    if let super::Authentication::SqlPassword { user, .. } = &report.request.authentication {
        r.name(Category::User, user);
    }
    for check in &report.checks {
        for field in &check.fields {
            if let Some(category) = field.category {
                r.name(category, &field.value);
            }
        }
    }
    r
}

fn status_label(coverage: Coverage) -> &'static str {
    match coverage {
        Coverage::Passed => "PASSED",
        Coverage::Diagnosed => "DIAGNOSED",
        Coverage::Classified => "CLASSIFIED",
        Coverage::Inconclusive => "INCONCLUSIVE",
        Coverage::Skipped => "SKIPPED",
        Coverage::NotApplicable => "N/A",
    }
}

fn skip_text(skip: &Skip) -> String {
    match skip {
        Skip::BlockedBy(check) => format!("blocked by {}", check.title()),
        Skip::Canceled => "canceled before it finished".to_string(),
        Skip::NotInThisBuild => "not checked by this build".to_string(),
    }
}

fn skip_reason(skip: &Skip) -> &'static str {
    match skip {
        Skip::BlockedBy(_) => "blockedByPrerequisite",
        Skip::Canceled => "canceled",
        Skip::NotInThisBuild => "notCheckedByThisBuild",
    }
}

/// A duration for the text report. Work that took under a millisecond reads
/// as `<1 ms` rather than `0 ms`, which looks like it did not run.
fn ms_text(ms: u64) -> String {
    if ms == 0 {
        "<1 ms".to_string()
    } else {
        format!("{ms} ms")
    }
}

fn settings_text(sources: &SettingSources) -> Option<String> {
    if sources.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut add = |label: &str, origin: &Option<SettingOrigin>| {
        if let Some(origin) = origin {
            if origin.source == super::SettingSource::Default {
                parts.push(format!("{label} default"));
            } else {
                parts.push(format!("{label} from {}", origin.text_name()));
            }
        }
    };
    add("server", &sources.server);
    add("login", &sources.user);
    add("authentication", &sources.authentication);
    add("database", &sources.database);
    add("encrypt", &sources.encrypt);
    add("certificate trust", &sources.trust_server_certificate);
    add("certificate name", &sources.host_name_in_certificate);
    add("time limit", &sources.login_timeout);
    Some(parts.join(", "))
}

/// What a check found, in a few words, for the text report.
fn check_text(check: &Check, r: &mut Redactor) -> String {
    if let Some(skip) = &check.skip {
        return skip_text(skip);
    }
    let field = |key: &str| check.field(key).unwrap_or_default().to_string();
    let named = |r: &mut Redactor, key: &str| {
        check
            .fields
            .iter()
            .filter(|f| f.key == key)
            .map(|f| match f.category {
                Some(category) => r.name(category, &f.value),
                None => f.value.clone(),
            })
            .collect::<Vec<_>>()
    };
    match check.id {
        super::CheckId::ConnectionInput => {
            if let Some(error) = check.field("error") {
                return error.to_string();
            }
            let mut parts = vec![field("protocol")];
            parts.push(format!("host {}", named(r, "host").join("")));
            let instance = named(r, "instance");
            if !instance.is_empty() {
                parts.push(format!("instance {}", instance.join("")));
            }
            if let Some(port) = check.field("port") {
                parts.push(format!("port {port}"));
            }
            let database = named(r, "database");
            if !database.is_empty() {
                parts.push(format!("database {}", database.join("")));
            }
            parts.join(", ")
        }
        // Not run by this build: skipped, so described above.
        super::CheckId::NameResolution
        | super::CheckId::InstanceResolution
        | super::CheckId::TcpConnect
        | super::CheckId::ConnectionAttempt
        | super::CheckId::SessionValidation => String::new(),
    }
}

/// The report as text, each line ending in `\n`.
pub fn text(report: &Report) -> String {
    let mut r = redactor(report);
    let request = &report.request;
    let mut out = String::new();

    if report.execution_status == super::ExecutionStatus::InvalidInvocation {
        out.push_str("sqlcmd diagnose: the request is invalid; nothing ran.\n\n");
    } else {
        let _ = writeln!(
            out,
            "sqlcmd diagnose: {}",
            r.name(Category::Server, &request.server)
        );
        let _ = write!(
            out,
            "Depth: {}{}; {} login",
            request.depth.title(),
            if request.depth_selected {
                ""
            } else {
                " (default)"
            },
            request.authentication.name()
        );
        if let super::Authentication::SqlPassword { user, .. } = &request.authentication {
            let _ = write!(out, " {}", r.name(Category::User, user));
        }
        let _ = write!(out, "; encrypt {}", request.encrypt.name());
        if let Some(database) = &request.database {
            let _ = write!(out, "; database {}", r.name(Category::Database, database));
        }
        if request.trust_server_certificate {
            out.push_str(if request.encrypt == super::Encrypt::Strict {
                "; -C ignored (strict encryption validates the certificate)"
            } else {
                "; server certificate trusted (-C)"
            });
        }
        if let Some(name) = &request.host_name_in_certificate {
            let _ = write!(
                out,
                "; certificate name {} (-F)",
                r.name(Category::Host, name)
            );
        }
        out.push('\n');
        let _ = writeln!(
            out,
            "Client: {}; time limit {} s per step (-l), {} s for SQL Server Browser",
            os_version().unwrap_or_else(|| os_family().to_string()),
            request.timeout_ms() / 1000,
            super::BROWSER_TIMEOUT_MS / 1000
        );
        out.push_str(if r.reveals() {
            "NOT SHARE-SAFE: identifiers are shown as they are (--local-detail).\n\n"
        } else {
            "Share-safe: identifiers are shown as labels (host-1, ...); --local-detail shows them.\n\n"
        });

        if !report.checks.is_empty() {
            let _ = writeln!(
                out,
                "  {:<20}  {:<12}  {:>8}  Details",
                "Check", "Result", "Time"
            );
        }
        for check in &report.checks {
            let duration = check.duration_ms.map_or(String::new(), ms_text);
            let detail = check_text(check, &mut r);
            let line = format!(
                "  {:<20}  {:<12}  {:>8}  {detail}",
                check.id.title(),
                status_label(check.coverage),
                duration
            );
            let _ = writeln!(out, "{}", line.trim_end());
            // Where each resolved setting came from, under the details.
            if check.id == super::CheckId::ConnectionInput
                && let Some(settings) = settings_text(&request.sources)
            {
                let _ = writeln!(out, "{:48}{settings}", "");
            }
        }
        if !report.checks.is_empty() {
            out.push('\n');
        }
    }

    let category = report.exit_category();
    let _ = writeln!(
        out,
        "Outcome:   {} (exit code {}: {})",
        report.outcome.name(),
        category.code(),
        category.meaning()
    );
    let _ = writeln!(
        out,
        "Execution: {}; deepest phase evaluated: {}",
        report.execution_status.name(),
        report
            .deepest_phase_evaluated()
            .map_or("none", Depth::title)
    );

    if !report.findings.is_empty() {
        out.push_str("\nFindings:\n");
        for finding in &report.findings {
            let _ = writeln!(
                out,
                "  [{}] {}",
                finding.certainty.name(),
                r.text(&finding.text)
            );
        }
        // With errors, the note follows them; it covers these findings too.
        if report.errors.is_empty() && !r.reveals() && quotes_messages(report) {
            out.push_str(
                "  Review findings before sharing: they may hold names sqlcmd cannot recognize.\n",
            );
        }
    }
    if let Some(domain) = &report.domain {
        let _ = write!(out, "\nSpecialist area: {}", domain.primary);
        if !domain.candidates.is_empty() {
            let _ = write!(out, " (candidates: {})", domain.candidates.join(", "));
        }
        if domain.diagnosed {
            out.push('\n');
        } else {
            let _ = writeln!(out, " - {SPECIALIST_NOTE}");
        }
    }
    if !report.errors.is_empty() {
        out.push_str("\nErrors, in order:\n");
        // A transport the client moved past is in the JSON report only.
        for error in report.errors.iter().filter(|e| !e.superseded) {
            let mut ids = Vec::new();
            if let Some(number) = error.number {
                ids.push(format!("error {number}"));
            }
            if let (Some(state), Some(class)) = (error.state, error.class) {
                ids.push(format!("state {state}, class {class}"));
            }
            if let Some(os) = error.os_error {
                ids.push(format!("os error {os}"));
            }
            if let Some(code) = &error.code {
                ids.push(code.clone());
            }
            let ids = if ids.is_empty() {
                String::new()
            } else {
                format!(" [{}]", ids.join("; "))
            };
            let _ = writeln!(out, "  {}{ids}: {}", error.source, r.text(&error.message));
        }
        if !r.reveals() {
            out.push_str("  Review error messages before sharing: they may hold names sqlcmd cannot recognize.\n");
        }
    }
    if !report.limitations.is_empty() {
        out.push_str("\nLimitations:\n");
        for limitation in &report.limitations {
            let _ = writeln!(out, "  - {limitation}");
        }
    }
    out
}

/// How deep a check, a finding or an error sits: the report, its array, then
/// the entry. Each is written on one line.
const ONE_LINE_DEPTH: usize = 3;

/// The report as one JSON document, ending in `\n`. `version` is sqlcmd's.
pub fn json(report: &Report, version: &str) -> Result<String, serde_json::Error> {
    let mut r = redactor(report);
    let request = &report.request;
    let share_safe = !r.reveals();
    let user = match &request.authentication {
        super::Authentication::SqlPassword { user, .. } => Some(r.name(Category::User, user)),
        super::Authentication::Integrated => None,
    };
    let category = report.exit_category();
    let document = ReportView {
        contract_version: CONTRACT_VERSION,
        coverage_matrix_version: super::COVERAGE_MATRIX_VERSION,
        command: "diagnose",
        sqlcmd: SqlcmdView {
            version,
            platform: PLATFORM,
        },
        share_safe,
        requested_depth: request.depth.name(),
        requested_depth_is_default: !request.depth_selected,
        deepest_phase_evaluated: report.deepest_phase_evaluated().map(Depth::name),
        target: TargetView {
            server: r.name(Category::Server, &request.server),
            database: request
                .database
                .as_deref()
                .map(|database| r.name(Category::Database, database)),
            authentication: request.authentication.name(),
            user,
            encrypt: request.encrypt.name(),
            trust_server_certificate: request.trust_server_certificate,
            host_name_in_certificate: request
                .host_name_in_certificate
                .as_deref()
                .map(|name| r.name(Category::Host, name)),
        },
        client: ClientView {
            os: os_family(),
            os_version: os_version(),
            connection_client: "mssql-tds",
        },
        start_time: utc_timestamp(report.start_unix_ms),
        duration_ms: report.duration_ms,
        execution_status: report.execution_status.name(),
        diagnostic_outcome: report.outcome.name(),
        exit_code: category.code(),
        exit_category: ExitCategoryView {
            name: category.name(),
            meaning: category.meaning(),
        },
        checks: report
            .checks
            .iter()
            .map(|check| CheckView::new(check, &request.sources, &mut r))
            .collect(),
        findings: report
            .findings
            .iter()
            .map(|finding| FindingView {
                certainty: finding.certainty.name(),
                check: finding.check.name(),
                text: r.text(&finding.text),
            })
            .collect(),
        specialist_domain: report.domain.as_ref().map(|domain| DomainView {
            primary: domain.primary,
            candidates: &domain.candidates,
            note: (!domain.diagnosed).then_some(SPECIALIST_NOTE),
        }),
        errors: report
            .errors
            .iter()
            .map(|error| ErrorView {
                source: error.source,
                check: error.check.name(),
                number: error.number,
                state: error.state,
                class: error.class,
                os_error: error.os_error,
                code: error.code.as_deref(),
                message: r.text(&error.message),
                superseded: error.superseded,
                from_message_text: error.from_message_text,
            })
            .collect(),
        limitations: &report.limitations,
        // Free text can hold names sqlcmd cannot recognize (certificate
        // names, SPNs): flagged for review rather than trusted as share-safe.
        review_before_sharing: {
            let mut paths = Vec::new();
            if share_safe && !report.errors.is_empty() {
                paths.push("errors[].message");
            }
            if share_safe && quotes_messages(report) {
                paths.push("findings[].text");
            }
            paths
        },
    };
    to_json_string(&document, ONE_LINE_DEPTH)
}

/// Whether a finding quotes a message from outside sqlcmd.
fn quotes_messages(report: &Report) -> bool {
    report.findings.iter().any(|f| f.quotes_message)
}

// The report as serde writes it: field order is the contract's order, and
// optional fields are left out when absent (fields that are always present
// but may be unknown are `null`).

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportView<'a> {
    contract_version: &'static str,
    coverage_matrix_version: &'static str,
    command: &'static str,
    sqlcmd: SqlcmdView<'a>,
    share_safe: bool,
    requested_depth: &'static str,
    requested_depth_is_default: bool,
    deepest_phase_evaluated: Option<&'static str>,
    target: TargetView,
    client: ClientView,
    start_time: String,
    duration_ms: u64,
    execution_status: &'static str,
    diagnostic_outcome: &'static str,
    exit_code: i32,
    exit_category: ExitCategoryView,
    checks: Vec<CheckView>,
    findings: Vec<FindingView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    specialist_domain: Option<DomainView<'a>>,
    errors: Vec<ErrorView<'a>>,
    limitations: &'a [String],
    review_before_sharing: Vec<&'static str>,
}

#[derive(Serialize)]
struct SqlcmdView<'a> {
    version: &'a str,
    platform: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetView {
    server: String,
    database: Option<String>,
    authentication: &'static str,
    user: Option<String>,
    encrypt: &'static str,
    trust_server_certificate: bool,
    host_name_in_certificate: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourcesView {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<SettingSourceView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    database: Option<SettingSourceView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<SettingSourceView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authentication: Option<SettingSourceView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypt: Option<SettingSourceView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trust_server_certificate: Option<SettingSourceView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_name_in_certificate: Option<SettingSourceView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    login_timeout: Option<SettingSourceView>,
}

impl SourcesView {
    fn new(sources: &SettingSources) -> Self {
        Self {
            server: sources.server.as_ref().map(SettingSourceView::new),
            database: sources.database.as_ref().map(SettingSourceView::new),
            user: sources.user.as_ref().map(SettingSourceView::new),
            authentication: sources.authentication.as_ref().map(SettingSourceView::new),
            encrypt: sources.encrypt.as_ref().map(SettingSourceView::new),
            trust_server_certificate: sources
                .trust_server_certificate
                .as_ref()
                .map(SettingSourceView::new),
            host_name_in_certificate: sources
                .host_name_in_certificate
                .as_ref()
                .map(SettingSourceView::new),
            login_timeout: sources.login_timeout.as_ref().map(SettingSourceView::new),
        }
    }

    fn non_empty(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }

    fn is_empty(&self) -> bool {
        self.server.is_none()
            && self.database.is_none()
            && self.user.is_none()
            && self.authentication.is_none()
            && self.encrypt.is_none()
            && self.trust_server_certificate.is_none()
            && self.host_name_in_certificate.is_none()
            && self.login_timeout.is_none()
    }
}

#[derive(Serialize)]
struct SettingSourceView {
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

impl SettingSourceView {
    fn new(origin: &SettingOrigin) -> Self {
        Self {
            source: origin.source.name(),
            name: origin.name.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientView {
    os: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_version: Option<String>,
    connection_client: &'static str,
}

#[derive(Serialize)]
struct ExitCategoryView {
    name: &'static str,
    meaning: &'static str,
}

#[derive(Serialize)]
struct FindingView {
    certainty: &'static str,
    check: &'static str,
    text: String,
}

#[derive(Serialize)]
struct DomainView<'a> {
    primary: &'static str,
    candidates: &'a [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

/// Said of the specialist area unless a diagnosed check named it.
const SPECIALIST_NOTE: &str = "the area for a support handoff, not a confirmed cause.";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorView<'a> {
    source: &'static str,
    check: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_error: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
    message: String,
    /// From a transport the client then moved past; a normal run does not
    /// print it.
    #[serde(skip_serializing_if = "is_false")]
    superseded: bool,
    #[serde(skip_serializing_if = "is_false")]
    from_message_text: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One check. Its own fields (`protocol`, `host`, `port`, `addresses`, ...)
/// vary by check, so the entry is written by hand, in this order: the check,
/// its depth and status, its duration, why it was skipped, its fields, then
/// where its settings came from (connection input).
struct CheckView {
    check: &'static str,
    depth: &'static str,
    status: &'static str,
    duration_ms: Option<u64>,
    deadline_ms: Option<u64>,
    skip: Option<SkipView>,
    fields: Vec<(&'static str, FieldValue)>,
    sources: Option<SourcesView>,
}

enum SkipView {
    BlockedBy(&'static str),
    Other {
        reason: &'static str,
        detail: String,
    },
}

/// A check's field: a list for `addresses`, a number for `port` when the
/// value is one, otherwise text.
#[derive(Serialize)]
#[serde(untagged)]
enum FieldValue {
    List(Vec<String>),
    Number(i64),
    Text(String),
}

impl CheckView {
    fn new(check: &Check, sources: &SettingSources, r: &mut Redactor) -> Self {
        // A key can repeat (one `addresses` field per address); its values
        // are gathered under the first occurrence.
        let mut fields: Vec<(&'static str, FieldValue)> = Vec::new();
        for field in &check.fields {
            if fields.iter().any(|(key, _)| *key == field.key) {
                continue;
            }
            let values: Vec<String> = check
                .fields
                .iter()
                .filter(|f| f.key == field.key)
                .map(|f| match f.category {
                    Some(category) => r.name(category, &f.value),
                    None => f.value.clone(),
                })
                .collect();
            let value = if field.key == "addresses" {
                FieldValue::List(values)
            } else if let (true, Ok(number)) = (field.key == "port", values[0].parse::<i64>()) {
                FieldValue::Number(number)
            } else {
                FieldValue::Text(values[0].clone())
            };
            fields.push((field.key, value));
        }
        Self {
            check: check.id.name(),
            depth: check.id.depth().name(),
            status: check.coverage.name(),
            duration_ms: check.duration_ms,
            deadline_ms: check.deadline_ms,
            skip: check.skip.as_ref().map(|skip| match skip {
                Skip::BlockedBy(by) => SkipView::BlockedBy(by.name()),
                skip => SkipView::Other {
                    reason: skip_reason(skip),
                    detail: skip_text(skip),
                },
            }),
            fields,
            sources: (check.id == super::CheckId::ConnectionInput)
                .then(|| SourcesView::new(sources))
                .and_then(SourcesView::non_empty),
        }
    }
}

impl Serialize for CheckView {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("check", self.check)?;
        map.serialize_entry("depth", self.depth)?;
        map.serialize_entry("status", self.status)?;
        if let Some(ms) = self.duration_ms {
            map.serialize_entry("durationMs", &ms)?;
        }
        if let Some(ms) = self.deadline_ms {
            map.serialize_entry("deadlineMs", &ms)?;
        }
        match &self.skip {
            Some(SkipView::BlockedBy(by)) => {
                map.serialize_entry("reason", "blockedByPrerequisite")?;
                map.serialize_entry("blockedBy", by)?;
            }
            Some(SkipView::Other { reason, detail }) => {
                map.serialize_entry("reason", reason)?;
                map.serialize_entry("detail", detail)?;
            }
            None => {}
        }
        for (key, value) in &self.fields {
            map.serialize_entry(key, value)?;
        }
        if let Some(sources) = &self.sources {
            map.serialize_entry("sources", sources)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::super::target::{Protocol, Target};
    use super::super::{
        Certainty, CheckId, DiagnosticOutcome, Domain, ExecutionStatus, Field, Finding,
    };
    use super::*;

    fn check(id: CheckId, coverage: Coverage, ms: u64, fields: Vec<Field>) -> Check {
        Check {
            id,
            coverage,
            duration_ms: Some(ms),
            deadline_ms: None,
            fields,
            skip: None,
            domain: None,
        }
    }

    fn passing_report() -> Report {
        let request = crate::diagnostics::tests::request("tcp:db01,1433", Depth::ConnectionInput);
        Report {
            request,
            start_unix_ms: 1_791_000_000_123,
            duration_ms: 1,
            target: Some(Target {
                protocol: Protocol::Tcp,
                host: "db01".to_string(),
                instance: None,
                port: Some(1433),
            }),
            checks: vec![check(
                CheckId::ConnectionInput,
                Coverage::Passed,
                0,
                vec![
                    Field::plain("protocol", "tcp"),
                    Field::identifier("host", "db01", Category::Host),
                    Field::plain("port", "1433"),
                ],
            )],
            findings: Vec::new(),
            errors: Vec::new(),
            domain: None,
            execution_status: ExecutionStatus::Completed,
            outcome: DiagnosticOutcome::Passed,
            limitations: vec!["A limitation.".to_string()],
        }
    }

    /// A server sqlcmd cannot parse, as [`crate::diagnostics::run`] reports it.
    fn failing_report() -> Report {
        crate::diagnostics::run(crate::diagnostics::tests::request(
            "tcp:db01,99999",
            Depth::SessionValidation,
        ))
    }

    #[test]
    fn text_lists_every_check_and_the_outcome() {
        let report = passing_report();
        let full = text(&report);
        // The client line names this machine's operating system.
        let client = full
            .lines()
            .find(|line| line.starts_with("Client: "))
            .unwrap_or_default();
        let limit = format!(
            "; time limit {} s per step (-l), 2 s for SQL Server Browser",
            report.request.login_timeout_seconds
        );
        assert!(client.ends_with(&limit), "{full}");
        let text = full.replacen(&format!("{client}\n"), "", 1);
        let expected = "\
sqlcmd diagnose: server-1
Depth: Connection input; SqlPassword login user-1; encrypt mandatory; server certificate trusted (-C)
Share-safe: identifiers are shown as labels (host-1, ...); --local-detail shows them.

  Check                 Result            Time  Details
  Connection input      PASSED           <1 ms  tcp, host host-1, port 1433

Outcome:   passed (exit code 0: every check at the requested depth passed)
Execution: completed; deepest phase evaluated: Connection input

Limitations:
  - A limitation.
";
        assert_eq!(text, expected);
    }

    #[test]
    fn text_explains_a_failure_without_revealing_names() {
        let text = text(&failing_report());
        for line in [
            "  Connection input      DIAGNOSED        <1 ms  invalidPort\n",
            "  Name resolution       SKIPPED                 blocked by Connection input\n",
            "Outcome:   issueDetected (exit code 1: the diagnosis identified an issue)\n",
            "Execution: completed; deepest phase evaluated: Connection input\n",
            "[confirmed] The server given to -S could not be parsed: `99999` is not a port number from 1 to 65535.\n",
            "\nSpecialist area: client.inputConfiguration\n",
            "  sqlcmd [invalidPort]: `99999` is not a port number from 1 to 65535\n",
            "Review error messages before sharing",
        ] {
            assert!(text.contains(line), "missing {line:?} in {text}");
        }
        for secret in ["db01", "secret-password", "'sa'"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }

    #[test]
    fn local_detail_shows_names_and_is_not_share_safe() {
        let mut report = failing_report();
        report.request.local_detail = true;
        let text = text(&report);
        assert!(text.contains("NOT SHARE-SAFE"), "{text}");
        assert!(text.contains("sqlcmd diagnose: tcp:db01,99999\n"), "{text}");
        let json = json(&report, "v").unwrap();
        assert!(json.contains("\"shareSafe\": false"), "{json}");
        assert!(json.contains("\"server\": \"tcp:db01,99999\""), "{json}");
        assert!(json.contains("\"reviewBeforeSharing\": []"), "{json}");
    }

    #[test]
    fn json_carries_the_specification_fields() {
        let json = json(&passing_report(), "18.7.0001.1").unwrap();
        for expected in [
            "\"contractVersion\": \"1.0\"",
            "\"command\": \"diagnose\"",
            "\"shareSafe\": true",
            "\"requestedDepth\": \"connectionInput\"",
            "\"deepestPhaseEvaluated\": \"connectionInput\"",
            "\"server\": \"server-1\"",
            "\"user\": \"user-1\"",
            "\"executionStatus\": \"completed\"",
            "\"diagnosticOutcome\": \"passed\"",
            "\"exitCode\": 0",
            "{ \"check\": \"connectionInput\", \"depth\": \"connectionInput\", \"status\": \"passed\", \"durationMs\": 0, \"protocol\": \"tcp\", \"host\": \"host-1\", \"port\": 1433 }",
            "\"findings\": []",
            "\"errors\": []",
            "\"reviewBeforeSharing\": []",
        ] {
            assert!(json.contains(expected), "missing {expected} in {json}");
        }
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["exitCategory"],
            serde_json::json!({
                "name": "passed",
                "meaning": "every check at the requested depth passed"
            })
        );
        assert!(!json.contains("db01"), "{json}");
    }

    /// A check this build does not run yet is skipped, and says so.
    #[test]
    fn a_check_this_build_does_not_run_says_so() {
        let mut report = passing_report();
        report.checks.push(Check::skipped(
            CheckId::ConnectionAttempt,
            Skip::NotInThisBuild,
        ));
        assert!(
            text(&report).contains(
                "  Connection attempt    SKIPPED                 not checked by this build\n"
            ),
            "{}",
            text(&report)
        );
        let json = json(&report, "v").unwrap();
        assert!(
            json.contains("{ \"check\": \"connectionAttempt\", \"depth\": \"connectionAttempt\", \"status\": \"skipped\", \"reason\": \"notCheckedByThisBuild\", \"detail\": \"not checked by this build\" }"),
            "{json}"
        );
    }

    /// A finding that quotes an outside message is flagged even without
    /// errors (shared memory set aside before TCP connected), in text and
    /// JSON alike; one of sqlcmd's own sentences is not.
    #[test]
    fn a_finding_quoting_a_message_is_flagged_for_review_on_its_own() {
        let mut report = passing_report();
        report.findings = vec![Finding {
            certainty: Certainty::Informational,
            check: CheckId::ConnectionAttempt,
            text: "Shared memory to this computer did not open (pipe busy).".to_string(),
            quotes_message: true,
        }];
        assert!(report.errors.is_empty());
        let value: serde_json::Value = serde_json::from_str(&json(&report, "v").unwrap()).unwrap();
        assert_eq!(
            value["reviewBeforeSharing"],
            serde_json::json!(["findings[].text"])
        );
        assert!(text(&report).contains("Review findings before sharing"));

        report.findings[0].quotes_message = false;
        let value: serde_json::Value = serde_json::from_str(&json(&report, "v").unwrap()).unwrap();
        assert_eq!(value["reviewBeforeSharing"], serde_json::json!([]));
        assert!(!text(&report).contains("before sharing"));

        report.request.local_detail = true;
        report.findings[0].quotes_message = true;
        let value: serde_json::Value = serde_json::from_str(&json(&report, "v").unwrap()).unwrap();
        assert_eq!(value["reviewBeforeSharing"], serde_json::json!([]));
    }

    #[test]
    fn a_diagnosed_specialist_area_is_not_called_unconfirmed() {
        let mut report = failing_report();
        report.domain = Some(Domain {
            primary: "security.authentication.sql",
            candidates: Vec::new(),
            diagnosed: true,
        });
        let text = text(&report);
        assert!(
            text.contains("\nSpecialist area: security.authentication.sql\n"),
            "{text}"
        );
        assert!(!text.contains("not a confirmed cause"), "{text}");
        let json = json(&report, "v").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["specialistDomain"],
            serde_json::json!({ "primary": "security.authentication.sql", "candidates": [] })
        );
    }

    #[test]
    fn json_reports_a_skip_with_what_blocked_it() {
        let json = json(&failing_report(), "v").unwrap();
        assert!(
            json.contains("{ \"check\": \"nameResolution\", \"depth\": \"endpointResolution\", \"status\": \"skipped\", \"reason\": \"blockedByPrerequisite\", \"blockedBy\": \"connectionInput\" }"),
            "{json}"
        );
        assert!(
            json.contains("\"diagnosticOutcome\": \"issueDetected\""),
            "{json}"
        );
        assert!(json.contains("{ \"source\": \"sqlcmd\", \"check\": \"connectionInput\", \"code\": \"invalidPort\", \"message\": \"`99999` is not a port number from 1 to 65535\" }"), "{json}");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Diagnosed: the area is not called unconfirmed.
        assert_eq!(
            value["specialistDomain"],
            serde_json::json!({ "primary": "client.inputConfiguration", "candidates": [] })
        );
        // Its finding is sqlcmd's own sentence: only the errors.
        assert_eq!(
            value["reviewBeforeSharing"],
            serde_json::json!(["errors[].message"])
        );
    }

    /// Unless a diagnosed check named it, the specialist area is said to be
    /// the area for a support handoff, not a confirmed cause.
    #[test]
    fn an_undiagnosed_specialist_area_is_called_unconfirmed() {
        let mut report = failing_report();
        report.domain = Some(Domain {
            primary: "undetermined",
            candidates: vec!["connectivity.network", "target.sqlServer"],
            diagnosed: false,
        });
        let text = text(&report);
        assert!(
            text.contains(
                "Specialist area: undetermined (candidates: connectivity.network, target.sqlServer) - the area for a support handoff, not a confirmed cause.\n"
            ),
            "{text}"
        );
        let value: serde_json::Value = serde_json::from_str(&json(&report, "v").unwrap()).unwrap();
        assert_eq!(
            value["specialistDomain"]["note"],
            "the area for a support handoff, not a confirmed cause."
        );
    }

    #[test]
    fn json_writes_each_check_finding_and_error_on_one_line() {
        let json = json(&failing_report(), "v").unwrap();
        assert!(
            json.contains("\n  \"checks\": [\n    { \"check\": \"connectionInput\""),
            "{json}"
        );
        assert!(
            json.contains("\n  \"findings\": [\n    { \"certainty\": \"confirmed\""),
            "{json}"
        );
        assert!(
            json.contains("\n  \"errors\": [\n    { \"source\": \"sqlcmd\""),
            "{json}"
        );
        assert!(json.contains("\"exitCategory\": {\n    \"name\""), "{json}");
    }

    #[test]
    fn json_says_whether_the_depth_was_the_default() {
        let mut report = passing_report();
        for selected in [false, true] {
            report.request.depth_selected = selected;
            let value: serde_json::Value =
                serde_json::from_str(&json(&report, "v").unwrap()).unwrap();
            assert_eq!(value["requestedDepthIsDefault"], !selected);
            assert_eq!(text(&report).contains("(default)"), !selected);
        }
    }

    #[test]
    fn an_invalid_request_reports_why() {
        let mut request = crate::diagnostics::tests::request("db01", Depth::SessionValidation);
        request.invalid = Some("-Q cannot be used: diagnose runs no queries of yours".to_string());
        let report = crate::diagnostics::run(request);
        let text = text(&report);
        assert!(
            text.starts_with("sqlcmd diagnose: the request is invalid; nothing ran.\n"),
            "{text}"
        );
        assert!(
            text.contains("exit code 6: the request was invalid; nothing ran"),
            "{text}"
        );
        let json = json(&report, "v").unwrap();
        assert!(
            json.contains("\"executionStatus\": \"invalidInvocation\""),
            "{json}"
        );
        assert!(
            json.contains("\"diagnosticOutcome\": \"notEvaluated\""),
            "{json}"
        );
        assert!(json.contains("\"checks\": []"), "{json}");
    }

    #[test]
    fn the_password_never_appears() {
        for report in [passing_report(), failing_report()] {
            for rendered in [text(&report), json(&report, "v").unwrap()] {
                assert!(!rendered.contains("secret-password"), "{rendered}");
            }
        }
    }

    #[test]
    fn the_certificate_name_is_a_host_like_any_other() {
        let mut report = passing_report();
        report.request.host_name_in_certificate = Some("db01.contoso.com".to_string());
        let text = text(&report);
        assert!(text.contains("; certificate name host-2 (-F)"), "{text}");
        let json = json(&report, "v").unwrap();
        assert!(
            json.contains("\"hostNameInCertificate\": \"host-2\""),
            "{json}"
        );
        for rendered in [&text, &json] {
            assert!(!rendered.contains("contoso"), "{rendered}");
        }
        report.request.local_detail = true;
        let json = super::json(&report, "v").unwrap();
        assert!(
            json.contains("\"hostNameInCertificate\": \"db01.contoso.com\""),
            "{json}"
        );
    }

    #[test]
    fn json_carries_deadlines_the_client_and_the_coverage_matrix() {
        let mut report = passing_report();
        let json_without = json(&report, "v").unwrap();
        assert!(
            json_without.contains("\"check\": \"connectionInput\", \"depth\": \"connectionInput\", \"status\": \"passed\", \"durationMs\": 0, \"protocol\""),
            "a check without a time limit has no deadline: {json_without}"
        );
        report.checks[0].deadline_ms = Some(8000);
        let json = json(&report, "v").unwrap();
        assert!(
            json.contains("\"coverageMatrixVersion\": \"1.0\""),
            "{json}"
        );
        assert!(
            json.contains("\"status\": \"passed\", \"durationMs\": 0, \"deadlineMs\": 8000,"),
            "{json}"
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
            assert!(value["client"]["osVersion"].is_string(), "{json}");
        }
    }
}

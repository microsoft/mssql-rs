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
//! Depth: Session validation (default); SqlPassword login user-1; encrypt mandatory; server certificate trusted (-C)
//! Share-safe: identifiers are shown as labels (host-1, ...); --local-detail shows them.
//!
//!   Check                 Result          Time  Details
//!   Connection input      PASSED         <1 ms  tcp, host host-1, port 1433
//!   Name resolution       PASSED          9 ms  host-1: address-1, address-2
//!   Instance resolution   N/A                   not a named instance
//!   TCP connect           PASSED          2 ms  port 1433: address-1 connected, address-2 refused
//!   Connection attempt    PASSED        160 ms  pre-login <1 ms, TLS 13 ms, login 136 ms; encryption mandatory
//!   Session validation    PASSED          3 ms  sqlServer (engine edition 3)
//!
//! Outcome:   passed (exit code 0: every check at the requested depth passed)
//! Execution: completed; deepest phase evaluated: Session validation
//! Server:    SQL Server 17.0.4085 (server-2), database database-1, packet size 8000, encrypted
//! ```

use std::fmt::Write;

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use super::redact::{Category, Redactor};
use super::stages::Step;
use super::{Check, Coverage, Depth, Report, Skip};
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
        for attempt in &check.attempts {
            r.name(Category::Address, &attempt.address.to_string());
        }
    }
    if let Some(server) = &report.server {
        if let Some(name) = &server.name {
            r.name(Category::Server, name);
        }
        r.name(Category::Database, &server.database);
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
        Skip::PortRequired => {
            "needs a confirmed port: give an explicit host,port for the named instance".to_string()
        }
        Skip::NotApplicable(why) => (*why).to_string(),
        Skip::Canceled => "canceled before it finished".to_string(),
    }
}

fn skip_reason(skip: &Skip) -> &'static str {
    match skip {
        Skip::BlockedBy(_) => "blockedByPrerequisite",
        Skip::PortRequired => "explicitPortRequired",
        Skip::NotApplicable(_) => "notApplicable",
        Skip::Canceled => "canceled",
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

fn phases_text(phases: &[Step]) -> String {
    phases
        .iter()
        .map(|p| {
            format!(
                "{} {}{}",
                p.stage
                    .title()
                    .to_lowercase()
                    .replace("tls handshake", "TLS"),
                ms_text(p.duration_ms),
                if p.ok { "" } else { " FAILED" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
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
        super::CheckId::NameResolution => {
            let host = named(r, "host").join("");
            let addresses = named(r, "addresses");
            if addresses.is_empty() {
                format!("{host}: {}", field("result"))
            } else {
                format!("{host}: {}", addresses.join(", "))
            }
        }
        super::CheckId::InstanceResolution => match check.field("port") {
            Some(port) => format!(
                "{}\\{}: port {port}",
                named(r, "host").join(""),
                named(r, "instance").join("")
            ),
            None if check.field("pipe").is_some() => format!(
                "{}\\{}: pipe {}",
                named(r, "host").join(""),
                named(r, "instance").join(""),
                named(r, "pipe").join("")
            ),
            None => format!(
                "{}\\{}: no usable port",
                named(r, "host").join(""),
                named(r, "instance").join("")
            ),
        },
        super::CheckId::TcpConnect => {
            let attempts: Vec<String> = check
                .attempts
                .iter()
                .map(|a| {
                    let os = a.os_error.map_or(String::new(), |e| format!(" ({e})"));
                    format!(
                        "{} {}{os}",
                        r.name(Category::Address, &a.address.to_string()),
                        a.result.name()
                    )
                })
                .collect();
            format!("port {}: {}", field("port"), attempts.join(", "))
        }
        super::CheckId::ConnectionAttempt => {
            let mut text = phases_text(&check.subphases);
            if let Some(encryption) = check.field("encryption") {
                let _ = write!(text, "; encryption {encryption}");
            }
            if let Some(tls) = check.field("tlsFailure") {
                let _ = write!(text, "; TLS failure: {tls}");
            }
            let spn = named(r, "spn");
            if !spn.is_empty() {
                let _ = write!(text, "; SPN {}", spn.join(""));
            }
            if let Some(cache) = check.field("kerberosTicketCache") {
                let _ = write!(text, "; ticket cache {cache}");
            }
            text
        }
        super::CheckId::SessionValidation => {
            let mut text = match check.field("targetType") {
                Some(target) => format!(
                    "{target} (engine edition {})",
                    check.field("engineEdition").unwrap_or("?")
                ),
                None => String::new(),
            };
            if let Some(offset) = check.field("clockOffsetMs") {
                let _ = write!(text, "; server clock {offset} ms from the client's");
            }
            text
        }
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
            out.push_str("; server certificate trusted (-C)");
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
    if let Some(auth) = report.authentication {
        let _ = writeln!(
            out,
            "Login:     {} {}",
            request.authentication.name(),
            auth.name()
        );
    }
    if let Some(server) = &report.server {
        let _ = write!(out, "Server:    SQL Server");
        if let Some(version) = &server.version {
            let _ = write!(out, " {version}");
        }
        if let Some(name) = &server.name {
            let _ = write!(out, " ({})", r.name(Category::Server, name));
        }
        let _ = write!(
            out,
            ", database {}, packet size {}, {}",
            r.name(Category::Database, &server.database),
            server.packet_size,
            if server.encrypted {
                "encrypted"
            } else {
                "only the login encrypted"
            }
        );
        if let Some(target_type) = server.target_type() {
            let _ = write!(out, ", {target_type}");
        }
        out.push('\n');
    }

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
    }
    if let Some(domain) = &report.domain {
        let _ = write!(out, "\nSpecialist area: {}", domain.primary);
        if !domain.candidates.is_empty() {
            let _ = write!(out, " (candidates: {})", domain.candidates.join(", "));
        }
        out.push_str(" - the area for a support handoff, not a confirmed cause.\n");
    }
    if let Some(guide) = report.tracing_guide {
        let _ = writeln!(
            out,
            "Driver tracing can show more than these checks; see {guide}"
        );
    }
    if !report.errors.is_empty() {
        out.push_str("\nErrors, in order:\n");
        for error in &report.errors {
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
            .map(|check| CheckView::new(check, &mut r))
            .collect(),
        authentication: report.authentication.map(|auth| AuthenticationView {
            method: request.authentication.name(),
            outcome: auth.name(),
        }),
        server: report.server.as_ref().map(|server| ServerView {
            name: server
                .name
                .as_deref()
                .map(|name| r.name(Category::Server, name)),
            version: server.version.as_deref(),
            database: r.name(Category::Database, &server.database),
            packet_size: server.packet_size,
            encrypted: server.encrypted,
            target_type: server.target_type(),
        }),
        findings: report
            .findings
            .iter()
            .map(|finding| FindingView {
                certainty: finding.certainty.name(),
                check: finding.check.name(),
                text: r.text(&finding.text),
            })
            .collect(),
        tracing_guide: report.tracing_guide,
        specialist_domain: report.domain.as_ref().map(|domain| DomainView {
            primary: domain.primary,
            candidates: &domain.candidates,
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
                from_message_text: error.from_message_text,
            })
            .collect(),
        limitations: &report.limitations,
        // Free text can hold names sqlcmd cannot recognize (certificate
        // names, SPNs): flagged for review rather than trusted as share-safe.
        review_before_sharing: if !report.errors.is_empty() && share_safe {
            vec!["errors[].message", "findings[].text"]
        } else {
            Vec::new()
        },
    };
    to_json_string(&document, ONE_LINE_DEPTH)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    authentication: Option<AuthenticationView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<ServerView<'a>>,
    findings: Vec<FindingView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tracing_guide: Option<&'static str>,
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
struct AuthenticationView {
    method: &'static str,
    outcome: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerView<'a> {
    name: Option<String>,
    version: Option<&'a str>,
    database: String,
    packet_size: u32,
    encrypted: bool,
    target_type: Option<&'static str>,
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
}

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
    #[serde(skip_serializing_if = "is_false")]
    from_message_text: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One check. Its own fields (`protocol`, `host`, `port`, `addresses`, ...)
/// vary by check, so the entry is written by hand, in this order: the check,
/// its depth and status, its duration, why it was skipped, its fields, then
/// its connection attempts and handshake phases.
struct CheckView {
    check: &'static str,
    depth: &'static str,
    status: &'static str,
    duration_ms: Option<u64>,
    deadline_ms: Option<u64>,
    skip: Option<SkipView>,
    fields: Vec<(&'static str, FieldValue)>,
    attempts: Vec<AttemptView>,
    phases: Vec<PhaseView>,
}

enum SkipView {
    BlockedBy(&'static str),
    Other {
        reason: &'static str,
        detail: String,
    },
}

/// A check's field: a list for `addresses`, a number for `port` and
/// `engineEdition` when the value is one, otherwise text.
#[derive(Serialize)]
#[serde(untagged)]
enum FieldValue {
    List(Vec<String>),
    Number(i64),
    Text(String),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttemptView {
    address: String,
    port: u16,
    result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_error: Option<i32>,
    duration_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseView {
    phase: &'static str,
    status: &'static str,
    duration_ms: u64,
}

impl CheckView {
    fn new(check: &Check, r: &mut Redactor) -> Self {
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
            } else if let (true, Ok(number)) = (
                matches!(field.key, "port" | "engineEdition" | "clockOffsetMs"),
                values[0].parse::<i64>(),
            ) {
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
            attempts: check
                .attempts
                .iter()
                .map(|attempt| AttemptView {
                    address: r.name(Category::Address, &attempt.address.to_string()),
                    port: attempt.port,
                    result: attempt.result.name(),
                    os_error: attempt.os_error,
                    duration_ms: attempt.duration_ms,
                })
                .collect(),
            phases: check
                .subphases
                .iter()
                .map(|phase| PhaseView {
                    phase: phase.stage.name(),
                    status: if phase.ok { "passed" } else { "failed" },
                    duration_ms: phase.duration_ms,
                })
                .collect(),
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
        if !self.attempts.is_empty() {
            map.serialize_entry("attempts", &self.attempts)?;
        }
        if !self.phases.is_empty() {
            map.serialize_entry("phases", &self.phases)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::super::stages::Stage;
    use super::super::target::{Protocol, Target};
    use super::super::{
        AuthOutcome, Certainty, CheckId, DiagnosticOutcome, Domain, ErrorRecord, ExecutionStatus,
        Field, Finding, ServerInfo, TcpAttempt, TcpResult,
    };
    use super::*;
    use std::time::Instant;

    fn check(id: CheckId, coverage: Coverage, ms: u64, fields: Vec<Field>) -> Check {
        Check {
            id,
            coverage,
            duration_ms: Some(ms),
            deadline_ms: None,
            fields,
            attempts: Vec::new(),
            subphases: Vec::new(),
            skip: None,
            domain: None,
        }
    }

    fn phase(stage: Stage, ms: u64) -> Step {
        let now = Instant::now();
        Step {
            stage,
            ok: true,
            duration_ms: ms,
            details: Vec::new(),
            started: now,
            ended: now,
        }
    }

    fn passing_report() -> Report {
        let mut request =
            crate::diagnostics::tests::request("tcp:db01,1433", Depth::SessionValidation);
        request.depth_selected = false;
        let mut tcp = check(
            CheckId::TcpConnect,
            Coverage::Passed,
            2,
            vec![Field::plain("port", "1433")],
        );
        tcp.attempts = vec![
            TcpAttempt {
                address: "10.0.0.1".parse().unwrap(),
                port: 1433,
                result: TcpResult::Connected,
                os_error: None,
                duration_ms: 1,
            },
            TcpAttempt {
                address: "::1".parse().unwrap(),
                port: 1433,
                result: TcpResult::Refused,
                os_error: Some(10061),
                duration_ms: 1,
            },
        ];
        let mut attempt = check(
            CheckId::ConnectionAttempt,
            Coverage::Passed,
            150,
            vec![Field::plain("encryption", "mandatory")],
        );
        attempt.subphases = vec![
            phase(Stage::Prelogin, 1),
            phase(Stage::Tls, 12),
            phase(Stage::Login, 130),
        ];
        Report {
            request,
            start_unix_ms: 1_791_000_000_123,
            duration_ms: 170,
            target: Some(Target {
                protocol: Protocol::Tcp,
                host: "db01".to_string(),
                instance: None,
                port: Some(1433),
            }),
            checks: vec![
                check(
                    CheckId::ConnectionInput,
                    Coverage::Passed,
                    0,
                    vec![
                        Field::plain("protocol", "tcp"),
                        Field::identifier("host", "db01", Category::Host),
                        Field::plain("port", "1433"),
                    ],
                ),
                check(
                    CheckId::NameResolution,
                    Coverage::Passed,
                    9,
                    vec![
                        Field::identifier("host", "db01", Category::Host),
                        Field::plain("result", "resolved"),
                        Field::identifier("addresses", "10.0.0.1", Category::Address),
                        Field::identifier("addresses", "::1", Category::Address),
                    ],
                ),
                Check {
                    duration_ms: None,
                    ..Check::skipped(
                        CheckId::InstanceResolution,
                        Skip::NotApplicable("not a named instance"),
                    )
                },
                tcp,
                attempt,
                check(
                    CheckId::SessionValidation,
                    Coverage::Passed,
                    3,
                    vec![
                        Field::plain("engineEdition", "3"),
                        Field::plain("targetType", "sqlServer"),
                    ],
                ),
            ],
            findings: Vec::new(),
            errors: Vec::new(),
            authentication: Some(AuthOutcome::Succeeded),
            server: Some(ServerInfo {
                name: Some("DB01".to_string()),
                version: Some("17.0.4085".to_string()),
                database: "master".to_string(),
                packet_size: 8000,
                encrypted: true,
                engine_edition: Some(3),
            }),
            domain: None,
            tracing_guide: None,
            execution_status: ExecutionStatus::Completed,
            outcome: DiagnosticOutcome::Passed,
            limitations: vec!["A limitation.".to_string()],
        }
    }

    fn failing_report() -> Report {
        let mut report = passing_report();
        report.checks.truncate(4);
        report.checks[3].coverage = Coverage::Diagnosed;
        report.checks[3].attempts[0].result = TcpResult::Refused;
        report.checks[3].attempts[0].os_error = Some(10061);
        report.checks.push(Check::skipped(
            CheckId::ConnectionAttempt,
            Skip::BlockedBy(CheckId::TcpConnect),
        ));
        report.authentication = None;
        report.server = None;
        report.outcome = DiagnosticOutcome::IssueDetected;
        report.domain = Some(Domain {
            primary: "undetermined",
            candidates: vec!["connectivity.network", "target.sqlServer"],
        });
        report.findings = vec![Finding {
            certainty: Certainty::Confirmed,
            check: CheckId::TcpConnect,
            text:
                "No address accepted a TCP connection on port 1433: 10.0.0.1 refused, ::1 refused."
                    .to_string(),
        }];
        let mut error = ErrorRecord::new(
            "os",
            CheckId::TcpConnect,
            "10.0.0.1:1433: connection refused by db01",
        );
        error.os_error = Some(10061);
        error.code = Some("refused".to_string());
        report.errors = vec![error];
        report
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
Depth: Session validation (default); SqlPassword login user-1; encrypt mandatory; server certificate trusted (-C)
Share-safe: identifiers are shown as labels (host-1, ...); --local-detail shows them.

  Check                 Result            Time  Details
  Connection input      PASSED           <1 ms  tcp, host host-1, port 1433
  Name resolution       PASSED            9 ms  host-1: address-1, address-2
  Instance resolution   N/A                     not a named instance
  TCP connect           PASSED            2 ms  port 1433: address-1 connected, address-2 refused (10061)
  Connection attempt    PASSED          150 ms  pre-login 1 ms, TLS 12 ms, login 130 ms; encryption mandatory
  Session validation    PASSED            3 ms  sqlServer (engine edition 3)

Outcome:   passed (exit code 0: every check at the requested depth passed)
Execution: completed; deepest phase evaluated: Session validation
Login:     SqlPassword succeeded
Server:    SQL Server 17.0.4085 (server-2), database database-1, packet size 8000, encrypted, sqlServer

Limitations:
  - A limitation.
";
        assert_eq!(text, expected);
    }

    #[test]
    fn text_explains_a_failure_without_revealing_names() {
        let text = text(&failing_report());
        assert!(
            text.contains(
                "  Connection attempt    SKIPPED                 blocked by TCP connect\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "Outcome:   issueDetected (exit code 1: the diagnosis identified an issue)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("Execution: completed; deepest phase evaluated: Network reachability\n"),
            "{text}"
        );
        assert!(text.contains("[confirmed] No address accepted a TCP connection on port 1433: address-1 refused, address-2 refused."), "{text}");
        assert!(
            text.contains(
                "Specialist area: undetermined (candidates: connectivity.network, target.sqlServer)"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "  os [os error 10061; refused]: address-1:1433: connection refused by host-1\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("Review error messages before sharing"),
            "{text}"
        );
        for secret in ["db01", "10.0.0.1", "secret-password", "'sa'"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }

    #[test]
    fn local_detail_shows_names_and_is_not_share_safe() {
        let mut report = failing_report();
        report.request.local_detail = true;
        let text = text(&report);
        assert!(text.contains("NOT SHARE-SAFE"), "{text}");
        assert!(text.contains("10.0.0.1 refused"), "{text}");
        assert!(!text.contains("secret-password"));
        let json = json(&report, "v").unwrap();
        assert!(json.contains("\"shareSafe\": false"), "{json}");
        assert!(json.contains("\"server\": \"tcp:db01,1433\""), "{json}");
        assert!(json.contains("\"reviewBeforeSharing\": []"), "{json}");
    }

    #[test]
    fn json_carries_the_specification_fields() {
        let json = json(&passing_report(), "18.7.0001.1").unwrap();
        for expected in [
            "\"contractVersion\": \"1.0\"",
            "\"command\": \"diagnose\"",
            "\"shareSafe\": true",
            "\"requestedDepth\": \"sessionValidation\"",
            "\"deepestPhaseEvaluated\": \"sessionValidation\"",
            "\"server\": \"server-1\"",
            "\"user\": \"user-1\"",
            "\"executionStatus\": \"completed\"",
            "\"diagnosticOutcome\": \"passed\"",
            "\"exitCode\": 0",
            "{ \"check\": \"connectionInput\", \"depth\": \"connectionInput\", \"status\": \"passed\", \"durationMs\": 0, \"protocol\": \"tcp\", \"host\": \"host-1\", \"port\": 1433 }",
            "\"addresses\": [\"address-1\", \"address-2\"]",
            "{ \"check\": \"instanceResolution\", \"depth\": \"endpointResolution\", \"status\": \"notApplicable\", \"reason\": \"notApplicable\", \"detail\": \"not a named instance\" }",
            "{ \"address\": \"address-2\", \"port\": 1433, \"result\": \"refused\", \"osError\": 10061, \"durationMs\": 1 }",
            "\"phases\": [{ \"phase\": \"prelogin\", \"status\": \"passed\", \"durationMs\": 1 }",
            "\"engineEdition\": 3",
            "\"name\": \"server-2\"",
            "\"targetType\": \"sqlServer\"",
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
        assert_eq!(
            value["authentication"],
            serde_json::json!({
                "method": "SqlPassword",
                "outcome": "succeeded"
            })
        );
        assert!(!json.contains("db01"), "{json}");
    }

    #[test]
    fn json_reports_a_skip_with_what_blocked_it() {
        let json = json(&failing_report(), "v").unwrap();
        assert!(
            json.contains("{ \"check\": \"connectionAttempt\", \"depth\": \"connectionAttempt\", \"status\": \"skipped\", \"reason\": \"blockedByPrerequisite\", \"blockedBy\": \"tcpConnect\" }"),
            "{json}"
        );
        assert!(
            json.contains("\"diagnosticOutcome\": \"issueDetected\""),
            "{json}"
        );
        assert!(json.contains("{ \"source\": \"os\", \"check\": \"tcpConnect\", \"osError\": 10061, \"code\": \"refused\", \"message\": \"address-1:1433: connection refused by host-1\" }"), "{json}");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["specialistDomain"],
            serde_json::json!({
                "primary": "undetermined",
                "candidates": ["connectivity.network", "target.sqlServer"]
            })
        );
        assert_eq!(
            value["reviewBeforeSharing"],
            serde_json::json!(["errors[].message", "findings[].text"])
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
            json.contains("\n  \"errors\": [\n    { \"source\": \"os\""),
            "{json}"
        );
        assert!(json.contains("\"exitCategory\": {\n    \"name\""), "{json}");
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
        for check in &mut report.checks {
            if check.id == CheckId::TcpConnect {
                check.deadline_ms = Some(8000);
            }
        }
        let json = json(&report, "v").unwrap();
        assert!(
            json.contains("\"coverageMatrixVersion\": \"1.0\""),
            "{json}"
        );
        assert!(
            json.contains("\"status\": \"passed\", \"durationMs\": 2, \"deadlineMs\": 8000,"),
            "{json}"
        );
        assert!(
            json.contains("\"check\": \"connectionInput\", \"depth\": \"connectionInput\", \"status\": \"passed\", \"durationMs\": 0, \"protocol\""),
            "a check without a time limit has no deadline: {json}"
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
            assert!(value["client"]["osVersion"].is_string(), "{json}");
        }
        assert!(value.get("tracingGuide").is_none(), "{json}");
    }

    #[test]
    fn kerberos_evidence_is_shown_share_safe() {
        let mut report = passing_report();
        for check in &mut report.checks {
            if check.id == CheckId::ConnectionAttempt {
                check.fields.push(Field::identifier(
                    "spn",
                    "MSSQLSvc/db01.contoso.com:1433",
                    Category::Spn,
                ));
                check
                    .fields
                    .push(Field::plain("kerberosTicketCache", "notFound"));
            }
        }
        let text = text(&report);
        assert!(
            text.contains("; SPN spn-1; ticket cache notFound"),
            "{text}"
        );
        let json = json(&report, "v").unwrap();
        assert!(
            json.contains("\"spn\": \"spn-1\", \"kerberosTicketCache\": \"notFound\""),
            "{json}"
        );
        assert!(!json.contains("contoso"), "{json}");
    }

    #[test]
    fn an_inconclusive_attempt_points_to_driver_tracing() {
        let mut report = failing_report();
        report.tracing_guide = Some(crate::diagnostics::TRACING_GUIDE);
        let text = text(&report);
        assert!(
            text.contains(
                "Driver tracing can show more than these checks; see https://aka.ms/sqlcmd-diagnose-tracing\n"
            ),
            "{text}"
        );
        let json = json(&report, "v").unwrap();
        assert!(
            json.contains("\"tracingGuide\": \"https://aka.ms/sqlcmd-diagnose-tracing\""),
            "{json}"
        );
    }
}

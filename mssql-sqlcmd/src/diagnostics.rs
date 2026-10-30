// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `sqlcmd diagnose`: checks, from the bottom up, that a connection to SQL
//! Server can be made, and reports where and why it fails.
//!
//! [`run`] follows the diagnostic depths of the sqlcmd specification, each
//! including the ones before it:
//!
//! | Depth | Checks |
//! |---|---|
//! | Connection input | parse the server given to `-S` ([`target`]); no external call |
//! | Endpoint resolution | the client resolves the host name, and asks SQL Server Browser for a named instance's port; it stops there |
//! | Network reachability | the client also opens its transport (a TCP connection to the first address that accepts, and to every address after it, or a pipe); it stops there |
//! | Connection attempt | the client also logs in, with its pre-login, TLS and login phases |
//! | Session validation | a minimal query on the new session (the engine edition and the server's clock) |
//!
//! Session validation, the deepest, is the default. Checks run best effort: a
//! failed check skips only the checks that need its result, each skipped check
//! names what blocked it, and every resolved address is tried. Each check
//! reports a coverage state (`passed`, `diagnosed`, `classified`,
//! `inconclusive`, `skipped`, `notApplicable`), its duration on a monotonic
//! clock, its time limit, and what it found. The [`Report`] adds the findings (confirmed,
//! suspected, informational), the ordered error chain with its stable
//! identifiers, the specialist domain for a handoff, the execution status, the
//! diagnostic outcome and the exit code. [`report`] renders it as text or JSON,
//! for review before sharing: database and user names become labels unless
//! local detail is requested ([`redact`]).
//!
//! sqlcmd makes no network call of its own. `mssql-tds` makes one connection
//! attempt, as a normal run does (its name resolution, SQL Server Browser
//! lookup, protocol order, TCP connects or pipe, TLS and login), through its
//! diagnostic entry point, which stops where the depth asks and also connects
//! to the addresses after the one it uses. Its connect-stage spans
//! ([`stages`]) are the evidence: the checks are read from them and from how
//! the attempt ended. The password is used to log in and is never part of the
//! report.

pub mod driver_text;
pub mod redact;
pub mod report;
pub mod stages;
pub mod target;

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::connect_stage;
use mssql_tds::connection::tds_client::{ResultSet, StatementResult, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::{
    DiagnosticStop, TdsConnectionProvider,
};
use mssql_tds::core::{EncryptionOptions, EncryptionSetting, Version};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::error::Error;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

use redact::Category;
use stages::{ConnectStagesOnly, Stage, StageRecorder, Step};
use target::{Protocol, Target};

/// How to log in. Its `Debug` output never shows the password.
#[derive(Clone, PartialEq, Eq)]
pub enum Authentication {
    /// SQL Server authentication (`-U`/`-P`).
    SqlPassword { user: String, password: String },
    /// Windows authentication, or Kerberos off Windows (`-E`).
    Integrated,
}

impl std::fmt::Debug for Authentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Authentication::SqlPassword { user, .. } => f
                .debug_struct("SqlPassword")
                .field("user", user)
                .finish_non_exhaustive(),
            Authentication::Integrated => f.write_str("Integrated"),
        }
    }
}
impl Authentication {
    /// The method's name, as the `--format json` document names it.
    pub fn name(&self) -> &'static str {
        match self {
            Authentication::SqlPassword { .. } => "SqlPassword",
            Authentication::Integrated => "Integrated",
        }
    }
}

/// Encryption, as sqlcmd's `-N` sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encrypt {
    /// `-No`: encrypt the login, and the rest only if the server requires it.
    Optional,
    /// `-N`/`-Nm`, sqlcmd's default: encrypt everything after pre-login.
    Mandatory,
    /// `-Ns`: TDS 8.0, encrypt everything, pre-login included.
    Strict,
}

impl Encrypt {
    pub fn name(self) -> &'static str {
        match self {
            Encrypt::Optional => "optional",
            Encrypt::Mandatory => "mandatory",
            Encrypt::Strict => "strict",
        }
    }
}

/// Where a resolved connection setting came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingSource {
    CommandLine,
    Environment,
    Default,
}

impl SettingSource {
    pub fn name(self) -> &'static str {
        match self {
            SettingSource::CommandLine => "commandLine",
            SettingSource::Environment => "environment",
            SettingSource::Default => "default",
        }
    }
}

/// The source for one resolved connection setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingOrigin {
    pub source: SettingSource,
    pub name: Option<String>,
}

impl SettingOrigin {
    pub fn command_line(name: impl Into<String>) -> Self {
        Self {
            source: SettingSource::CommandLine,
            name: Some(name.into()),
        }
    }

    pub fn environment(name: impl Into<String>) -> Self {
        Self {
            source: SettingSource::Environment,
            name: Some(name.into()),
        }
    }

    pub fn default_source() -> Self {
        Self {
            source: SettingSource::Default,
            name: None,
        }
    }

    pub fn text_name(&self) -> &str {
        self.name.as_deref().unwrap_or("default")
    }
}

/// Sources for resolved connection settings. Fields left as `None` are not
/// reported, so older callers that do not know about sources keep their output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingSources {
    pub server: Option<SettingOrigin>,
    pub database: Option<SettingOrigin>,
    pub user: Option<SettingOrigin>,
    pub authentication: Option<SettingOrigin>,
    pub encrypt: Option<SettingOrigin>,
    pub trust_server_certificate: Option<SettingOrigin>,
    pub host_name_in_certificate: Option<SettingOrigin>,
    pub login_timeout: Option<SettingOrigin>,
}

impl SettingSources {
    pub fn is_empty(&self) -> bool {
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

/// How far the diagnosis goes. Each depth includes the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Depth {
    ConnectionInput,
    EndpointResolution,
    NetworkReachability,
    ConnectionAttempt,
    SessionValidation,
}

impl Depth {
    pub const ALL: [Depth; 5] = [
        Depth::ConnectionInput,
        Depth::EndpointResolution,
        Depth::NetworkReachability,
        Depth::ConnectionAttempt,
        Depth::SessionValidation,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Depth::ConnectionInput => "connectionInput",
            Depth::EndpointResolution => "endpointResolution",
            Depth::NetworkReachability => "networkReachability",
            Depth::ConnectionAttempt => "connectionAttempt",
            Depth::SessionValidation => "sessionValidation",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Depth::ConnectionInput => "Connection input",
            Depth::EndpointResolution => "Endpoint resolution",
            Depth::NetworkReachability => "Network reachability",
            Depth::ConnectionAttempt => "Connection attempt",
            Depth::SessionValidation => "Session validation",
        }
    }
}

/// What to check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The server as given to `-S`.
    pub server: String,
    pub database: Option<String>,
    pub authentication: Authentication,
    pub encrypt: Encrypt,
    /// `-C`: accept the server's certificate without validating it.
    pub trust_server_certificate: bool,
    /// `-F`: the name the server's certificate must carry.
    pub host_name_in_certificate: Option<String>,
    /// `-l`, in seconds. 0 uses sqlcmd's default of 8.
    pub login_timeout_seconds: u32,
    /// The deepest checks to run.
    pub depth: Depth,
    /// Whether the user chose the depth (otherwise it is the default).
    pub depth_selected: bool,
    /// Show database and user names as they are, instead of labels.
    pub local_detail: bool,
    /// Why the request is invalid, when sqlcmd rejected it: nothing runs.
    pub invalid: Option<String>,
    /// Where resolved non-secret connection settings came from.
    pub sources: SettingSources,
}

impl Request {
    fn timeout(&self) -> Duration {
        Duration::from_secs(u64::from(match self.login_timeout_seconds {
            0 => 8,
            seconds => seconds,
        }))
    }

    fn timeout_ms(&self) -> u64 {
        u64::try_from(self.timeout().as_millis()).unwrap_or(u64::MAX)
    }
}

/// The version of the diagnostic coverage matrix this implementation follows
/// (`docs/diagnose-coverage.md`): which phases are checked, with which signals,
/// and which coverage states each can report.
pub const COVERAGE_MATRIX_VERSION: &str = "1.0";

/// The Microsoft Learn page on driver tracing for `sqlcmd diagnose`, given when
/// the client's own evidence cannot explain a failure.
pub const TRACING_GUIDE: &str = "https://aka.ms/sqlcmd-diagnose-tracing";

/// How long SQL Server Browser has to answer.
const BROWSER_TIMEOUT_MS: u64 = 2000;

/// The port the client connects a dedicated administrator connection to.
const DAC_PORT: u16 = 1434;

/// Why the TCP checks do not apply, when the client used a pipe.
const NOT_TCP: &str = "the connection does not use TCP";
/// Why the TCP checks do not apply, when the client used shared memory
/// without a protocol prefix.
const SHARED_MEMORY_FIRST: &str =
    "without a protocol prefix sqlcmd connects to this computer over shared memory first";
/// Why the TCP checks do not apply, when TCP did not connect and the pipe
/// the protocol order moved to opened.
const NOT_ON_THE_PATH: &str = "sqlcmd's connection did not use TCP";

/// Kerberos rejects tickets whose clock differs from the server's by more than
/// this, by default.
const KERBEROS_CLOCK_TOLERANCE_MS: i64 = 5 * 60 * 1000;

/// A check of the connection path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckId {
    ConnectionInput,
    NameResolution,
    InstanceResolution,
    TcpConnect,
    ConnectionAttempt,
    SessionValidation,
}

impl CheckId {
    const ORDER: [CheckId; 6] = [
        CheckId::ConnectionInput,
        CheckId::NameResolution,
        CheckId::InstanceResolution,
        CheckId::TcpConnect,
        CheckId::ConnectionAttempt,
        CheckId::SessionValidation,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CheckId::ConnectionInput => "connectionInput",
            CheckId::NameResolution => "nameResolution",
            CheckId::InstanceResolution => "instanceResolution",
            CheckId::TcpConnect => "tcpConnect",
            CheckId::ConnectionAttempt => "connectionAttempt",
            CheckId::SessionValidation => "sessionValidation",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            CheckId::ConnectionInput => "Connection input",
            CheckId::NameResolution => "Name resolution",
            CheckId::InstanceResolution => "Instance resolution",
            CheckId::TcpConnect => "TCP connect",
            CheckId::ConnectionAttempt => "Connection attempt",
            CheckId::SessionValidation => "Session validation",
        }
    }

    /// The depth the check belongs to.
    pub fn depth(self) -> Depth {
        match self {
            CheckId::ConnectionInput => Depth::ConnectionInput,
            CheckId::NameResolution | CheckId::InstanceResolution => Depth::EndpointResolution,
            CheckId::TcpConnect => Depth::NetworkReachability,
            CheckId::ConnectionAttempt => Depth::ConnectionAttempt,
            CheckId::SessionValidation => Depth::SessionValidation,
        }
    }
}

/// How conclusively a check ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// The check ran and its phase works.
    Passed,
    /// The check failed, and stable evidence identifies the failure.
    Diagnosed,
    /// The check failed, and the failure was classified from weaker evidence
    /// (e.g. error text).
    Classified,
    /// The check could not establish whether its phase works.
    Inconclusive,
    /// The check did not run; [`Check::skip`] says why.
    Skipped,
    /// The check does not apply to this target.
    NotApplicable,
}

impl Coverage {
    pub fn name(self) -> &'static str {
        match self {
            Coverage::Passed => "passed",
            Coverage::Diagnosed => "diagnosed",
            Coverage::Classified => "classified",
            Coverage::Inconclusive => "inconclusive",
            Coverage::Skipped => "skipped",
            Coverage::NotApplicable => "notApplicable",
        }
    }

    fn failed(self) -> bool {
        matches!(
            self,
            Coverage::Diagnosed | Coverage::Classified | Coverage::Inconclusive
        )
    }
}

/// Why a check did not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    /// An earlier check whose result it needs did not succeed.
    BlockedBy(CheckId),
    /// A named instance has no confirmed port: SQL Server Browser gave none.
    /// Instance resolution blocks the check, and asks for an explicit
    /// `host,port` ([REQUIRES_EXPLICIT_PORT]).
    PortRequired,
    /// Why the check does not apply.
    NotApplicable(&'static str),
    /// The diagnosis was canceled before the check finished.
    Canceled,
}

/// The field of a check that names what the user must give before the checks
/// it blocks can run.
pub(crate) const REQUIRES: &str = "requires";
/// [REQUIRES] on instance resolution when SQL Server Browser gave a named
/// instance no usable port: an explicit `host,port`.
pub(crate) const REQUIRES_EXPLICIT_PORT: &str = "explicitPort";

/// A value a check found, with the kind of identifier it is, if it is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub key: &'static str,
    pub value: String,
    pub category: Option<Category>,
}

impl Field {
    fn plain(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: value.into(),
            category: None,
        }
    }

    fn identifier(key: &'static str, value: impl Into<String>, category: Category) -> Self {
        Self {
            key,
            value: value.into(),
            category: Some(category),
        }
    }
}

/// What a TCP connect to one address came to, as the operating system
/// reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpResult {
    Connected,
    Refused,
    TimedOut,
    HostUnreachable,
    NetworkUnreachable,
    Reset,
    Failed,
    /// Not tried: the check's time limit was spent on earlier addresses.
    NotAttempted,
}

impl TcpResult {
    pub fn name(self) -> &'static str {
        match self {
            TcpResult::Connected => "connected",
            TcpResult::Refused => "refused",
            TcpResult::TimedOut => "timedOut",
            TcpResult::HostUnreachable => "hostUnreachable",
            TcpResult::NetworkUnreachable => "networkUnreachable",
            TcpResult::Reset => "reset",
            TcpResult::Failed => "failed",
            TcpResult::NotAttempted => "notAttempted",
        }
    }

    fn from_io(error: &std::io::Error) -> Self {
        Self::from_fields(
            Some(format!("{:?}", error.kind()).as_str()),
            error.raw_os_error(),
        )
    }

    /// The result from a failed connect's error kind (as `Debug` names it)
    /// and operating-system code, as the client's TCP stage records them.
    fn from_fields(kind: Option<&str>, os_error: Option<i32>) -> Self {
        match kind {
            Some("ConnectionRefused") => TcpResult::Refused,
            Some("TimedOut") => TcpResult::TimedOut,
            Some("HostUnreachable") => TcpResult::HostUnreachable,
            Some("NetworkUnreachable") => TcpResult::NetworkUnreachable,
            Some("ConnectionReset") => TcpResult::Reset,
            _ => os_error.map_or(TcpResult::Failed, tcp_result_from_code),
        }
    }
}

/// The connect result for an operating-system error code the error kind did
/// not name. The numbers differ by platform (61 is a refusal on macOS but
/// ENODATA on Linux), so each platform matches only its own.
fn tcp_result_from_code(code: i32) -> TcpResult {
    // (refused, timed out, host unreachable, network unreachable)
    let (refused, timed_out, host, network) = if cfg!(windows) {
        (10061, 10060, 10065, 10051)
    } else if cfg!(target_os = "macos") {
        (61, 60, 65, 51)
    } else {
        (111, 110, 113, 101)
    };
    match code {
        c if c == refused => TcpResult::Refused,
        c if c == timed_out => TcpResult::TimedOut,
        c if c == host => TcpResult::HostUnreachable,
        c if c == network => TcpResult::NetworkUnreachable,
        _ => TcpResult::Failed,
    }
}
/// One TCP connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpAttempt {
    /// The address, with its IPv6 scope when it has one (`fe80::1%12`).
    pub address: String,
    pub port: u16,
    pub result: TcpResult,
    pub os_error: Option<i32>,
    pub duration_ms: u64,
}

/// One check as it ran.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub id: CheckId,
    pub coverage: Coverage,
    pub duration_ms: Option<u64>,
    /// How long the check was allowed to take, when it had a time limit.
    pub deadline_ms: Option<u64>,
    pub fields: Vec<Field>,
    /// TCP connect: one per address.
    pub attempts: Vec<TcpAttempt>,
    /// Connection attempt: its pre-login, TLS and login phases.
    pub subphases: Vec<Step>,
    pub skip: Option<Skip>,
    /// The handoff area when the check failed.
    pub domain: Option<Domain>,
}

impl Check {
    fn new(id: CheckId, coverage: Coverage) -> Self {
        Self {
            id,
            coverage,
            duration_ms: None,
            deadline_ms: None,
            fields: Vec::new(),
            attempts: Vec::new(),
            subphases: Vec::new(),
            skip: None,
            domain: None,
        }
    }

    fn skipped(id: CheckId, skip: Skip) -> Self {
        let coverage = match skip {
            Skip::NotApplicable(_) => Coverage::NotApplicable,
            _ => Coverage::Skipped,
        };
        Self {
            skip: Some(skip),
            ..Self::new(id, coverage)
        }
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
    }
}

/// How certain a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Certainty {
    /// What was observed.
    Confirmed,
    /// A likely cause the evidence suggests but does not establish.
    Suspected,
    /// Context.
    Informational,
}

impl Certainty {
    pub fn name(self) -> &'static str {
        match self {
            Certainty::Confirmed => "confirmed",
            Certainty::Suspected => "suspected",
            Certainty::Informational => "informational",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub certainty: Certainty,
    pub check: CheckId,
    pub text: String,
    /// The text quotes a message from outside sqlcmd (the operating system,
    /// the TLS library, the client), which may hold names sqlcmd cannot
    /// recognize and so cannot label.
    pub quotes_message: bool,
}

/// One error of the ordered error chain, with the identifiers it came with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorRecord {
    /// `sqlcmd`, `os`, `sqlBrowser`, `tls`, `sqlServer` or `client`.
    pub source: &'static str,
    pub check: CheckId,
    /// SQL Server's error number.
    pub number: Option<u32>,
    pub state: Option<u8>,
    pub class: Option<i32>,
    /// The operating system's error code.
    pub os_error: Option<i32>,
    /// A stable symbolic identifier, e.g. `refused` or `invalidPort`.
    pub code: Option<String>,
    pub message: String,
    /// What a normal sqlcmd run prints for the same event, in the ODBC
    /// driver's wording (see [`driver_text`]); `None` when it has no equivalent.
    pub text: Option<String>,
    /// From a transport sqlcmd then moved past, without a protocol prefix
    /// (TCP before named pipes): kept as evidence, but a normal run, which
    /// reports only the last transport's errors, would not print it.
    pub superseded: bool,
    /// The classification relied on the message text: lower confidence.
    pub from_message_text: bool,
}

impl ErrorRecord {
    fn new(source: &'static str, check: CheckId, message: impl Into<String>) -> Self {
        Self {
            source,
            check,
            number: None,
            state: None,
            class: None,
            os_error: None,
            code: None,
            message: message.into(),
            text: None,
            superseded: false,
            from_message_text: false,
        }
    }
}

/// The technical area for a support handoff; not a confirmed root cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Domain {
    /// The most specific area the evidence supports, or `undetermined`.
    pub primary: &'static str,
    /// Possible areas when the primary is `undetermined`.
    pub candidates: Vec<&'static str>,
    /// The failed check that named it was diagnosed: its evidence is
    /// definite, so the report does not call the area unconfirmed.
    pub diagnosed: bool,
}

impl Domain {
    fn specific(primary: &'static str) -> Self {
        Self {
            primary,
            candidates: Vec::new(),
            diagnosed: false,
        }
    }

    fn undetermined(candidates: &[&'static str]) -> Self {
        Self {
            primary: "undetermined",
            candidates: candidates.to_vec(),
            diagnosed: false,
        }
    }
}

/// The outcome of the selected login method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    Succeeded,
    Failed,
    NotEvaluated,
}

impl AuthOutcome {
    pub fn name(self) -> &'static str {
        match self {
            AuthOutcome::Succeeded => "succeeded",
            AuthOutcome::Failed => "failed",
            AuthOutcome::NotEvaluated => "notEvaluated",
        }
    }
}

/// What the server reported about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// The instance name the server gave itself (`@@SERVERNAME`).
    pub name: Option<String>,
    /// e.g. `17.0.4085`.
    pub version: Option<String>,
    pub database: String,
    /// Whether everything after login stays encrypted.
    pub encrypted: bool,
    /// From session validation: `SERVERPROPERTY('EngineEdition')`.
    pub engine_edition: Option<i32>,
}

impl ServerInfo {
    /// The kind of target, from the engine edition (`SERVERPROPERTY('EngineEdition')`).
    /// Edition 11 is either Azure Synapse serverless SQL pool or Microsoft
    /// Fabric, which the edition alone cannot tell apart; an edition this
    /// version does not know is `unknown`, not guessed.
    pub fn target_type(&self) -> Option<&'static str> {
        self.engine_edition.map(|edition| match edition {
            1..=4 => "sqlServer",
            5 => "azureSqlDatabase",
            6 => "azureSynapseAnalytics",
            8 => "azureSqlManagedInstance",
            9 => "azureSqlEdge",
            11 => "azureSynapseServerlessOrFabric",
            12 => "fabricSqlDatabase",
            _ => "unknown",
        })
    }
}

/// Whether sqlcmd carried out the diagnosis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionStatus {
    Completed,
    Partial,
    Canceled,
    Failed,
    InvalidInvocation,
}

impl ExecutionStatus {
    pub fn name(self) -> &'static str {
        match self {
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Partial => "partial",
            ExecutionStatus::Canceled => "canceled",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::InvalidInvocation => "invalidInvocation",
        }
    }
}

/// What the diagnosis found about the connection, at the requested depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticOutcome {
    Passed,
    IssueDetected,
    Inconclusive,
    NotEvaluated,
}

impl DiagnosticOutcome {
    pub fn name(self) -> &'static str {
        match self {
            DiagnosticOutcome::Passed => "passed",
            DiagnosticOutcome::IssueDetected => "issueDetected",
            DiagnosticOutcome::Inconclusive => "inconclusive",
            DiagnosticOutcome::NotEvaluated => "notEvaluated",
        }
    }
}

/// The process exit category, in the specification's precedence. Each has its
/// own exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitCategory {
    Passed,
    IssueDetected,
    Inconclusive,
    Partial,
    Canceled,
    Failed,
    InvalidInvocation,
}

impl ExitCategory {
    pub fn code(self) -> i32 {
        match self {
            ExitCategory::Passed => 0,
            ExitCategory::IssueDetected => 1,
            ExitCategory::Inconclusive => 2,
            ExitCategory::Partial => 3,
            ExitCategory::Canceled => 4,
            ExitCategory::Failed => 5,
            ExitCategory::InvalidInvocation => 6,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ExitCategory::Passed => "passed",
            ExitCategory::IssueDetected => "issueDetected",
            ExitCategory::Inconclusive => "inconclusive",
            ExitCategory::Partial => "partial",
            ExitCategory::Canceled => "canceled",
            ExitCategory::Failed => "failed",
            ExitCategory::InvalidInvocation => "invalidInvocation",
        }
    }

    pub fn meaning(self) -> &'static str {
        match self {
            ExitCategory::Passed => "every check at the requested depth passed",
            ExitCategory::IssueDetected => "the diagnosis identified an issue",
            ExitCategory::Inconclusive => {
                "the diagnosis could not establish whether the connection works"
            }
            ExitCategory::Partial => "some checks that should have run could not complete",
            ExitCategory::Canceled => "the diagnosis was canceled",
            ExitCategory::Failed => "sqlcmd failed internally; the summary is not trustworthy",
            ExitCategory::InvalidInvocation => "the request was invalid; nothing ran",
        }
    }
}

/// The outcome of [`run`].
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// The request diagnosed, without its password.
    pub request: Request,
    pub start_unix_ms: u64,
    pub duration_ms: u64,
    pub target: Option<Target>,
    pub checks: Vec<Check>,
    pub findings: Vec<Finding>,
    pub errors: Vec<ErrorRecord>,
    pub authentication: Option<AuthOutcome>,
    pub server: Option<ServerInfo>,
    pub domain: Option<Domain>,
    /// Where to read about driver tracing, when the client's own evidence ran
    /// out (see [`TRACING_GUIDE`]).
    pub tracing_guide: Option<&'static str>,
    pub execution_status: ExecutionStatus,
    pub outcome: DiagnosticOutcome,
    pub limitations: Vec<String>,
}

impl Report {
    /// The deepest depth at which a check ran (or was found not to apply).
    pub fn deepest_phase_evaluated(&self) -> Option<Depth> {
        self.checks
            .iter()
            .filter(|c| c.coverage != Coverage::Skipped)
            .map(|c| c.id.depth())
            .max()
    }

    pub fn exit_category(&self) -> ExitCategory {
        match self.execution_status {
            ExecutionStatus::InvalidInvocation => ExitCategory::InvalidInvocation,
            ExecutionStatus::Failed => ExitCategory::Failed,
            ExecutionStatus::Canceled => ExitCategory::Canceled,
            ExecutionStatus::Partial => ExitCategory::Partial,
            ExecutionStatus::Completed => match self.outcome {
                DiagnosticOutcome::IssueDetected => ExitCategory::IssueDetected,
                DiagnosticOutcome::Inconclusive | DiagnosticOutcome::NotEvaluated => {
                    ExitCategory::Inconclusive
                }
                DiagnosticOutcome::Passed => ExitCategory::Passed,
            },
        }
    }

    /// sqlcmd's exit code for this outcome.
    pub fn exit_code(&self) -> i32 {
        self.exit_category().code()
    }
}

/// The diagnosis state, one word so it changes atomically: bit 0 a cancel,
/// bit 1 a run in progress, the other bits the run's number. A cancel is
/// kept only while a run is in progress, and a run starts and ends with none,
/// so no cancel is cleared once the run has started, and none is carried
/// into the next run (the number keeps a late one from landing on it).
static STATE: AtomicU64 = AtomicU64::new(0);
const CANCEL_REQUESTED: u64 = 0b01;
const IN_PROGRESS: u64 = 0b10;

/// Held by [`run`]: diagnoses run one at a time.
pub(crate) static RUNNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Cancels the diagnosis [`run`] is running, from any thread (a console or
/// signal handler included: it only updates an atomic). The checks that
/// finished are kept, the others are skipped, and the report says it was
/// canceled. With no diagnosis running it does nothing.
pub fn cancel() {
    cancel_seen(STATE.load(Ordering::SeqCst));
}

/// Cancels the run that was in progress when `seen` was read, if it still
/// is: a cancel never lands on a later run. True when it was canceled.
fn cancel_seen(seen: u64) -> bool {
    let mut state = seen;
    while state & IN_PROGRESS != 0 && state >> 2 == seen >> 2 {
        match STATE.compare_exchange_weak(
            state,
            state | CANCEL_REQUESTED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return true,
            Err(now) => state = now,
        }
    }
    false
}

/// Marks a run in progress, with a new number and no cancel. Called with
/// [`RUNNING`] held.
fn begin_run() {
    let number = (STATE.load(Ordering::SeqCst) >> 2).wrapping_add(1);
    STATE.store((number << 2) | IN_PROGRESS, Ordering::SeqCst);
}

/// Ends the run in progress, and with it any cancel. Called with [`RUNNING`]
/// held.
fn end_run() {
    let number = STATE.load(Ordering::SeqCst) >> 2;
    STATE.store(number << 2, Ordering::SeqCst);
}

fn cancel_requested() -> bool {
    STATE.load(Ordering::SeqCst) & CANCEL_REQUESTED != 0
}

/// Runs the diagnosis `request` describes and reports it. Blocks until every
/// check at the requested depth has run or been skipped, or [`cancel`] is
/// called. Diagnoses run one at a time (a second call waits for the first).
pub fn run(request: Request) -> Report {
    // One diagnosis at a time, so a run never ends or receives another's cancel.
    let _only = RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    struct InProgress;
    impl Drop for InProgress {
        fn drop(&mut self) {
            end_run();
        }
    }
    begin_run();
    let _in_progress = InProgress;
    run_until(request, &cancel_requested)
}

/// [`run`], canceled once `canceled` returns true.
fn run_until(request: Request, canceled: &dyn Fn() -> bool) -> Report {
    run_until_on(request, canceled, cfg!(windows))
}

/// [`run_until`], parsing the server as the client does on Windows
/// (`windows`) or elsewhere, so tests check either platform's.
fn run_until_on(request: Request, canceled: &dyn Fn() -> bool, windows: bool) -> Report {
    let start_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let start = Instant::now();
    let mut diagnosis = Diagnosis::new(request);
    diagnosis.windows = windows;

    if let Some(reason) = diagnosis.request.invalid.clone() {
        diagnosis.finding(
            Certainty::Confirmed,
            CheckId::ConnectionInput,
            reason.clone(),
        );
        let mut record = ErrorRecord::new("sqlcmd", CheckId::ConnectionInput, reason);
        record.code = Some("invalidInvocation".to_string());
        diagnosis.errors.push(record);
        diagnosis.status = ExecutionStatus::InvalidInvocation;
    } else {
        let subscriber = tracing_subscriber::registry()
            .with(diagnosis.recorder.clone().with_filter(ConnectStagesOnly));
        tracing::subscriber::with_default(subscriber, || {
            // One thread: every span mssql-tds opens is seen by the subscriber
            // set on it.
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => block_on_and_leave(runtime, async {
                    // The cancel first: one already received is not lost to
                    // a check that finishes at once.
                    let stopped = tokio::select! {
                        biased;
                        () = wait_until(canceled) => true,
                        () = diagnosis.execute() => false,
                    };
                    if stopped {
                        diagnosis.canceled().await;
                    }
                }),
                Err(error) => {
                    diagnosis.status = ExecutionStatus::Failed;
                    diagnosis.limitations.push(format!(
                        "The diagnosis could not start its async runtime: {error}."
                    ));
                }
            }
        });
    }
    diagnosis.finish(
        start_unix_ms,
        u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    )
}

/// Runs `future` to completion, then shuts `runtime` down without waiting
/// for blocking work still in progress: a name lookup or pipe wait the
/// operating system has not answered keeps going on its own thread, so a
/// canceled or timed-out diagnosis returns at once.
fn block_on_and_leave<F: std::future::Future>(
    runtime: tokio::runtime::Runtime,
    future: F,
) -> F::Output {
    let output = runtime.block_on(future);
    runtime.shutdown_background();
    output
}
/// Returns once `canceled` returns true, checking it every 50 ms.
async fn wait_until(canceled: &dyn Fn() -> bool) {
    while !canceled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
/// The state of one diagnosis while it runs.
struct Diagnosis {
    request: Request,
    /// Whether Connection input parses as the client does on Windows, which
    /// alone takes LocalDB, named pipes and shared memory: this platform's,
    /// and either in tests.
    windows: bool,
    /// How the client's Kerberos ticket cache is looked for: None on
    /// Windows, which keeps tickets in the logon session; tests set either.
    ticket_cache: Option<fn() -> (&'static str, String)>,
    recorder: StageRecorder,
    target: Option<Target>,
    checks: Vec<Check>,
    findings: Vec<Finding>,
    errors: Vec<ErrorRecord>,
    authentication: Option<AuthOutcome>,
    server: Option<ServerInfo>,
    /// The SPN the client requested with integrated authentication, as it
    /// recorded it.
    spn: Option<String>,
    /// The transport the run ended on when it is a pipe: the one the protocol
    /// order reached without a prefix (connection attempt), or the pipe that did
    /// not open below that depth. `None` follows the target's own protocol.
    transport: Option<Protocol>,
    limitations: Vec<String>,
    status: ExecutionStatus,
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// What the client's connection attempt came to.
enum Ended {
    Connected,
    /// It stopped where the depth asked it to.
    Stopped,
    Failed(Error),
    /// The run was canceled before it ended: the stages it left unfinished
    /// were abandoned, which is not a failure.
    Canceled,
}

impl Ended {
    /// Whether a time limit ended it: a stage it abandoned then ran out of
    /// time.
    fn timed_out(&self) -> bool {
        matches!(self, Ended::Failed(Error::TimeoutError(_)))
    }

    fn canceled(&self) -> bool {
        matches!(self, Ended::Canceled)
    }
}

/// One transport the client tried, from its connect stages: TCP (the host's
/// name resolution and its connects) or a pipe.
struct Transport<'a> {
    dns: Option<&'a Step>,
    tcp: Vec<&'a Step>,
    pipe: Option<&'a Step>,
}

impl Transport<'_> {
    fn is_tcp(&self) -> bool {
        self.pipe.is_none()
    }

    fn opened(&self) -> bool {
        match self.pipe {
            Some(pipe) => pipe.ok == Some(true),
            None => self.tcp.iter().any(|t| t.ok == Some(true)),
        }
    }

    fn started(&self) -> Option<Instant> {
        self.dns
            .or(self.pipe)
            .or_else(|| self.tcp.first().copied())
            .map(|s| s.started)
    }

    /// The transport, as the target names transports.
    fn protocol(&self) -> Protocol {
        match self.pipe {
            None => Protocol::Tcp,
            Some(pipe) => match pipe.detail(connect_stage::KIND) {
                Some(connect_stage::KIND_SHARED_MEMORY) => Protocol::SharedMemory,
                Some(connect_stage::KIND_LOCALDB) => Protocol::LocalDb,
                _ => Protocol::NamedPipe,
            },
        }
    }
}

/// The transports the stages show, in the order they were tried. `steps`
/// are in the order they started; the connects to the addresses after the
/// one used start after it, and belong to its TCP path.
fn transports<'a>(steps: &[&'a Step]) -> Vec<Transport<'a>> {
    let mut list: Vec<Transport<'a>> = Vec::new();
    for &step in steps {
        match step.stage {
            Stage::Dns => list.push(Transport {
                dns: Some(step),
                tcp: Vec::new(),
                pipe: None,
            }),
            Stage::Tcp => match list.last_mut() {
                Some(transport) if transport.is_tcp() => transport.tcp.push(step),
                _ => list.push(Transport {
                    dns: None,
                    tcp: vec![step],
                    pipe: None,
                }),
            },
            Stage::Pipe => list.push(Transport {
                dns: None,
                tcp: Vec::new(),
                pipe: Some(step),
            }),
            _ => {}
        }
    }
    list
}

/// What a failed instance lookup says about the name: the Browser's host did
/// not resolve.
fn lookup_name_failure(lookup: &Step) -> bool {
    lookup.failed()
        && (matches!(
            lookup.detail(connect_stage::RESULT),
            Some("nameResolutionFailed" | "noAddresses")
        ) || resolver_timed_out(lookup))
}

/// A Browser lookup whose resolving the server used up its time limit, so
/// SQL Server Browser was never asked: `timedOut` before any address (a
/// Browser that does not answer is `noAnswer`).
fn resolver_timed_out(lookup: &Step) -> bool {
    lookup.failed()
        && lookup.detail(connect_stage::RESULT) == Some("timedOut")
        && lookup.detail(connect_stage::ADDRESSES).is_none()
}

/// The addresses a stage lists, without repeats.
fn listed_addresses(step: &Step) -> Vec<String> {
    let mut addresses: Vec<String> = Vec::new();
    for address in step
        .detail(connect_stage::ADDRESSES)
        .unwrap_or_default()
        .split(", ")
        .filter(|a| !a.is_empty())
    {
        if !addresses.iter().any(|a| a == address) {
            addresses.push(address.to_string());
        }
    }
    addresses
}

impl Diagnosis {
    fn new(request: Request) -> Self {
        Self {
            request,
            windows: cfg!(windows),
            ticket_cache: (!cfg!(windows)).then_some(kerberos_ticket_cache as fn() -> _),
            recorder: StageRecorder::new(),
            target: None,
            checks: Vec::new(),
            findings: Vec::new(),
            errors: Vec::new(),
            authentication: None,
            server: None,
            spn: None,
            transport: None,
            limitations: Vec::new(),
            status: ExecutionStatus::Completed,
        }
    }

    /// The run was canceled: the stages the client finished before the
    /// cancel still give their checks, and every check at the requested
    /// depth that did not finish is skipped as canceled.
    async fn canceled(&mut self) {
        let steps = self.recorder.steps();
        self.canceled_after(steps).await;
    }

    /// [Diagnosis::canceled], with the stages the client recorded before
    /// the cancel.
    async fn canceled_after(&mut self, steps: Vec<Step>) {
        self.status = ExecutionStatus::Canceled;
        self.observe_so_far(steps).await;
        // The cancel decides the status: replaying the stages (a local
        // failure makes a run partial) does not downgrade it.
        self.status = ExecutionStatus::Canceled;
        for id in CheckId::ORDER {
            if self.wants(id.depth()) && !self.checks.iter().any(|c| c.id == id) {
                self.checks.push(Check::skipped(id, Skip::Canceled));
            }
        }
        // A check skipped only because one the cancel cut short did not
        // finish was canceled too.
        let canceled: Vec<CheckId> = self
            .checks
            .iter()
            .filter(|c| c.skip == Some(Skip::Canceled))
            .map(|c| c.id)
            .collect();
        for check in &mut self.checks {
            if let Some(Skip::BlockedBy(by)) = check.skip
                && canceled.contains(&by)
            {
                check.skip = Some(Skip::Canceled);
            }
        }
    }

    /// Reads the checks from the stages the client recorded before a cancel,
    /// when the connection attempt had started and no check past the
    /// connection input was made from it yet.
    async fn observe_so_far(&mut self, steps: Vec<Step>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let dac_lookup = target.protocol == Protocol::Admin && target.instance.is_some();
        if dac_lookup
            || !self.wants(Depth::EndpointResolution)
            || self.checks.iter().any(|c| c.id != CheckId::ConnectionInput)
        {
            return;
        }
        if steps.is_empty() {
            return;
        }
        Box::pin(self.observe(&target, steps, &Ended::Canceled, 0, None)).await;
    }
    fn wants(&self, depth: Depth) -> bool {
        self.request.depth >= depth
    }

    fn finding(&mut self, certainty: Certainty, check: CheckId, text: impl Into<String>) {
        self.findings.push(Finding {
            certainty,
            check,
            text: text.into(),
            quotes_message: false,
        });
    }

    /// A finding whose text quotes a message from outside sqlcmd.
    fn quoting_finding(&mut self, certainty: Certainty, check: CheckId, text: impl Into<String>) {
        self.finding(certainty, check, text);
        if let Some(finding) = self.findings.last_mut() {
            finding.quotes_message = true;
        }
    }

    fn limitation(&mut self, text: &str) {
        if !self.limitations.iter().any(|l| l == text) {
            self.limitations.push(text.to_string());
        }
    }

    /// Skips every check after `after`, up to the requested depth, that has
    /// not run yet.
    fn skip_rest(&mut self, after: CheckId, skip: &Skip) {
        let position = CheckId::ORDER
            .iter()
            .position(|id| *id == after)
            .unwrap_or(0);
        for id in &CheckId::ORDER[position + 1..] {
            if self.wants(id.depth()) && !self.checks.iter().any(|c| c.id == *id) {
                self.checks.push(Check::skipped(*id, skip.clone()));
            }
        }
    }

    /// The client makes one connection attempt, as a normal run does, and
    /// the checks are read from the stages it went through: sqlcmd resolves
    /// nothing and connects to nothing itself. The depth sets where the
    /// attempt stops.
    async fn execute(&mut self) {
        let Some(target) = self.connection_input() else {
            return;
        };
        if !self.wants(Depth::EndpointResolution) {
            return;
        }
        // A named instance's DAC port comes from SQL Server Browser's DAC
        // request, which the client does not make: only the name is checked.
        let dac_lookup = target.protocol == Protocol::Admin && target.instance.is_some();
        let stop_after = match self.request.depth {
            _ if dac_lookup => Some(DiagnosticStop::Resolution),
            Depth::ConnectionInput | Depth::EndpointResolution => Some(DiagnosticStop::Resolution),
            Depth::NetworkReachability => Some(DiagnosticStop::Transport),
            Depth::ConnectionAttempt | Depth::SessionValidation => None,
        };
        let context = self.client_context();
        let server = self.request.server.trim().to_string();
        let started = Instant::now();
        // A bound around the client's own time limits, which are whole
        // seconds, and the connects to the other addresses that follow them.
        let limit = self.request.timeout() * 2 + Duration::from_secs(1);
        let attempt = bounded(
            limit,
            self.request.timeout().as_secs(),
            // Boxed: the client's connect future is large.
            Box::pin(TdsConnectionProvider::new().diagnose_client(context, &server, stop_after)),
        )
        .await;
        let attempt_ms = millis(started);
        let (client, ended) = match attempt {
            Ok(Some(client)) => (Some(client), Ended::Connected),
            Ok(None) => (None, Ended::Stopped),
            Err(error) => (None, Ended::Failed(error)),
        };
        let steps = self.recorder.steps();
        if dac_lookup {
            self.dac_lookup(&target, &steps);
            if let Some(mut client) = client {
                let _ = client.close_connection().await;
            }
            return;
        }
        Box::pin(self.observe(&target, steps, &ended, attempt_ms, client.as_ref())).await;
        match client {
            Some(mut client) => {
                if self.wants(Depth::SessionValidation) {
                    self.session_validation(&mut client).await;
                }
                let _ = client.close_connection().await;
            }
            None => self.skip_rest(
                CheckId::ConnectionAttempt,
                &Skip::BlockedBy(CheckId::ConnectionAttempt),
            ),
        }
    }

    /// `admin:host\instance`: the name is checked; the DAC port, which only
    /// SQL Server Browser's DAC request gives, is not guessed.
    fn dac_lookup(&mut self, target: &Target, steps: &[Step]) {
        let mut ordered: Vec<&Step> = steps.iter().collect();
        ordered.sort_by_key(|s| s.started);
        let groups = transports(&ordered);
        let lookup = ordered
            .iter()
            .copied()
            .find(|s| s.stage == Stage::InstanceLookup);
        self.name_resolution(target, groups.first(), lookup, None, false, false);
        let mut check = Check::new(CheckId::InstanceResolution, Coverage::Inconclusive);
        check.fields.push(Field::identifier(
            "host",
            target.resolvable_host().to_string(),
            Category::Host,
        ));
        check.fields.push(Field::identifier(
            "instance",
            target.instance.clone().unwrap_or_default(),
            Category::Instance,
        ));
        check
            .fields
            .push(Field::plain("result", "dacLookupUnsupported"));
        check.domain = Some(Domain::undetermined(&[
            "connectivity.instanceResolution",
            "client.driver",
        ]));
        self.checks.push(check);
        self.limitation(
            "The dedicated administrator connection to a named instance gets its port from \
             SQL Server Browser's DAC request, which the client does not make, so its endpoint \
             and connection are not checked.",
        );
        self.skip_rest(
            CheckId::InstanceResolution,
            &Skip::BlockedBy(CheckId::InstanceResolution),
        );
    }

    /// Reads the checks from the stages the attempt went through (`steps`)
    /// and how it `ended`. `attempt_ms` is how long it took; `client` is the
    /// connection when it logged in.
    async fn observe(
        &mut self,
        target: &Target,
        mut steps: Vec<Step>,
        ended: &Ended,
        attempt_ms: u64,
        client: Option<&TdsClient>,
    ) {
        steps.sort_by_key(|s| s.started);
        let timed_out = ended.timed_out();
        // After a routing redirect the client connected again where it was
        // sent: the checks before the attempt are of the target's connection.
        let redirect_at = steps
            .iter()
            .filter(|s| {
                s.stage == Stage::Login && s.detail(connect_stage::REDIRECTED) == Some("true")
            })
            .map(|s| s.ended)
            .min();
        let all: Vec<&Step> = steps.iter().collect();
        let before: Vec<&Step> = all
            .iter()
            .copied()
            .filter(|s| redirect_at.is_none_or(|at| s.started < at))
            .collect();
        let groups = transports(&before);
        let lookup = before
            .iter()
            .copied()
            .find(|s| s.stage == Stage::InstanceLookup);

        if groups.is_empty()
            && lookup.is_none()
            && let Ended::Failed(error) = ended
        {
            self.failed_before_any_stage(target, error, attempt_ms);
            return;
        }

        // Transports tried before SQL Server Browser was asked (shared
        // memory first, without a prefix) were moved past.
        let looked_up_after = lookup.map_or(0, |l| {
            groups
                .iter()
                .take_while(|g| g.started().is_some_and(|s| s < l.started))
                .count()
        });
        // The transport the attempt ended on, and the TCP path it tried (the
        // last one), which a later transport may have replaced.
        let primary_at = (groups.len() > looked_up_after).then(|| groups.len() - 1);
        let primary = primary_at.map(|i| &groups[i]);
        let tcp_at = groups[looked_up_after..]
            .iter()
            .rposition(Transport::is_tcp)
            .map(|i| i + looked_up_after);
        let tcp = tcp_at.map(|i| &groups[i]);
        let tcp_superseded = tcp_at.is_some_and(|i| Some(i) != primary_at);
        // Without any stage the client was about to open a pipe (shared
        // memory first, without a prefix) when it stopped.
        let first_pipe = match groups.first() {
            Some(first) if !first.is_tcp() => Some(first.protocol()),
            Some(_) => None,
            None if lookup.is_none() => expected_pipe(target, self.windows),
            None => None,
        };
        let not_tcp = (tcp.is_none() && lookup.is_none()).then(|| {
            if first_pipe == Some(Protocol::SharedMemory) && target.protocol == Protocol::Default {
                SHARED_MEMORY_FIRST
            } else {
                NOT_TCP
            }
        });

        self.moved_past(&groups, primary_at, &all);
        let pipe_used = primary.and_then(|g| g.pipe).filter(|p| p.ok == Some(true));
        // A pipe the protocol order moved to opened: the TCP path it moved
        // past is not the connection's, so what failed there does not count.
        let off_the_path = tcp_superseded && pipe_used.is_some();
        let (status, limitations) = (self.status, self.limitations.len());
        let name_failed =
            self.name_resolution(target, tcp, lookup, not_tcp, timed_out, tcp_superseded);
        // Without a prefix the client tries a local named instance over
        // shared memory before it asks SQL Server Browser, and a stop after
        // resolution comes before either: the instance was not looked up.
        let not_looked_up = groups.is_empty()
            && lookup.is_none()
            && matches!(ended, Ended::Stopped)
            && target.instance.is_some()
            && not_tcp == Some(SHARED_MEMORY_FIRST);
        let lookup_blocks = if not_looked_up {
            self.instance_not_looked_up(target);
            None
        } else {
            self.instance_resolution(target, lookup, tcp.is_some(), not_tcp, pipe_used, timed_out)
        };
        // SQL Server Browser gave a port, and nothing was tried after it:
        // the attempt ended (its time limit spent on the lookup, say) before
        // the TCP connect it needed.
        let lookup_port = lookup
            .filter(|l| l.ok == Some(true))
            .and_then(|l| l.detail(connect_stage::PORT));
        let tcp_not_reached = tcp.is_none()
            && groups.len() == looked_up_after
            && target.protocol.uses_tcp()
            && lookup_port.is_some();
        let tcp_blocks = if self.wants(Depth::NetworkReachability) {
            let blocked = lookup_blocks
                .clone()
                .or_else(|| name_failed.then_some(Skip::BlockedBy(CheckId::NameResolution)));
            let pipe_failed = primary.filter(|g| !g.is_tcp() && !g.opened());
            match (tcp, blocked, pipe_failed) {
                (None, None, _) if tcp_not_reached => self.tcp_not_reached(lookup_port, ended),
                // At the connection attempt's depth, that check reports it.
                (None, None, Some(pipe)) if !self.wants(Depth::ConnectionAttempt) => {
                    self.pipe_not_opened(pipe, ended)
                }
                // The protocol order moved past a failed TCP path to a pipe
                // that did not open either: the pipe's failure is the
                // connection's. The TCP path's checks are not on its path,
                // their errors kept as superseded evidence.
                (tcp, blocked, Some(pipe))
                    if tcp_superseded && !self.wants(Depth::ConnectionAttempt) =>
                {
                    self.tcp_connect(
                        target,
                        tcp,
                        blocked,
                        not_tcp,
                        tcp_superseded,
                        ended.canceled(),
                    );
                    self.not_on_the_path(NOT_ON_THE_PATH);
                    self.status = status;
                    self.limitations.truncate(limitations);
                    self.checks.retain(|c| c.id != CheckId::TcpConnect);
                    self.pipe_not_opened(pipe, ended)
                }
                (tcp, blocked, _) => self.tcp_connect(
                    target,
                    tcp,
                    blocked,
                    not_tcp,
                    tcp_superseded,
                    ended.canceled(),
                ),
            }
        } else {
            None
        };
        if off_the_path {
            self.not_on_the_path(NOT_ON_THE_PATH);
            self.status = status;
            self.limitations.truncate(limitations);
        }

        if !self.wants(Depth::ConnectionAttempt) {
            return;
        }
        // The attempt never got a transport to the target: the check that
        // failed blocks it.
        let blocked = match primary {
            Some(group) if group.is_tcp() && !group.opened() && redirect_at.is_none() => {
                Some(if group.dns.is_some_and(|d| d.ok != Some(true)) {
                    Skip::BlockedBy(CheckId::NameResolution)
                } else {
                    tcp_blocks.unwrap_or(Skip::BlockedBy(CheckId::TcpConnect))
                })
            }
            None => lookup_blocks.or(tcp_blocks),
            _ => None,
        };
        if let Some(skip) = blocked {
            self.checks
                .push(Check::skipped(CheckId::ConnectionAttempt, skip.clone()));
            self.skip_rest(CheckId::ConnectionAttempt, &skip);
            return;
        }
        if ended.canceled() {
            // The cancel cut the attempt short: its phases did not fail.
            self.checks
                .push(Check::skipped(CheckId::ConnectionAttempt, Skip::Canceled));
            return;
        }
        let all_groups = transports(&all);
        self.connection_attempt(
            target,
            &all,
            all_groups.last(),
            redirect_at.is_some(),
            ended,
            attempt_ms,
            client,
        )
        .await;
    }

    /// The attempt failed before any connect stage (resolving a LocalDB
    /// instance, say): the failure is the connection attempt's or, below
    /// that depth, the deepest check's.
    fn failed_before_any_stage(&mut self, target: &Target, error: &Error, attempt_ms: u64) {
        let network = [
            CheckId::NameResolution,
            CheckId::InstanceResolution,
            CheckId::TcpConnect,
        ];
        let carrier = if self.wants(Depth::ConnectionAttempt) {
            CheckId::ConnectionAttempt
        } else {
            network
                .iter()
                .rev()
                .copied()
                .find(|id| self.wants(id.depth()))
                .unwrap_or(CheckId::NameResolution)
        };
        for id in network {
            if id == carrier || !self.wants(id.depth()) {
                continue;
            }
            self.checks.push(if target.protocol.uses_tcp() {
                Check::skipped(id, Skip::BlockedBy(carrier))
            } else {
                Check::skipped(id, Skip::NotApplicable(NOT_TCP))
            });
        }
        let classified = classify_connection_error(error, None, &self.request.authentication);
        let mut check = Check::new(carrier, classified.coverage);
        check.duration_ms = Some(attempt_ms);
        check.deadline_ms = Some(self.request.timeout_ms());
        check.domain = Some(classified.domain);
        self.checks.push(check);
        if carrier == CheckId::ConnectionAttempt {
            self.authentication = Some(classified.auth);
        }
        self.quoting_finding(Certainty::Confirmed, carrier, classified.finding);
        self.errors
            .extend(classified.errors.into_iter().map(|mut record| {
                record.check = carrier;
                record
            }));
        self.skip_rest(carrier, &Skip::BlockedBy(carrier));
    }

    /// Name resolution, from the TCP path's `dns` stage or, before it, the
    /// instance lookup's. True when it failed.
    fn name_resolution(
        &mut self,
        target: &Target,
        tcp: Option<&Transport>,
        lookup: Option<&Step>,
        not_tcp: Option<&'static str>,
        timed_out: bool,
        superseded: bool,
    ) -> bool {
        let id = CheckId::NameResolution;
        let from_lookup = lookup
            .filter(|l| l.detail(connect_stage::ADDRESSES).is_some() || lookup_name_failure(l));
        let Some(step) = tcp.and_then(|t| t.dns).or(from_lookup) else {
            if lookup.is_some_and(Step::abandoned) && !timed_out {
                // A cancel cut short the lookup that resolves the name.
                self.checks.push(Check::skipped(id, Skip::Canceled));
                return false;
            }
            let reason = not_tcp.unwrap_or("the client did not resolve a name");
            self.checks
                .push(Check::skipped(id, Skip::NotApplicable(reason)));
            return false;
        };
        let host = step
            .detail(connect_stage::HOST)
            .or_else(|| step.detail(connect_stage::SERVER))
            .map_or_else(|| target.resolvable_host().into_owned(), str::to_string);
        let mut check = Check::new(id, Coverage::Passed);
        check
            .fields
            .push(Field::identifier("host", host.clone(), Category::Host));
        let addresses = listed_addresses(step);
        // The resolver failed and the client went on with addresses cached
        // from an earlier resolution: name resolution failed, while the TCP
        // connect is still checked with those addresses.
        let from_cache = step.stage == Stage::Dns
            && step.detail(connect_stage::SOURCE) == Some(connect_stage::SOURCE_FALLBACK_CACHE);
        if !addresses.is_empty() && !from_cache {
            let literal = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok();
            if literal {
                check.duration_ms = Some(0);
                check.fields.push(Field::plain("result", "addressLiteral"));
            } else {
                check.deadline_ms = Some(self.request.timeout_ms());
                // Within an instance lookup, resolving is not timed apart.
                check.duration_ms = (step.stage == Stage::Dns).then_some(step.duration_ms);
                check.fields.push(Field::plain("result", "resolved"));
            }
            for address in addresses {
                check
                    .fields
                    .push(Field::identifier("addresses", address, Category::Address));
            }
            self.checks.push(check);
            return false;
        }
        if step.abandoned() && !timed_out {
            self.checks.push(Check::skipped(id, Skip::Canceled));
            return true;
        }
        // Resolving for the Browser lookup has the lookup's time limit.
        let resolver_timeout = step.stage == Stage::InstanceLookup && resolver_timed_out(step);
        check.deadline_ms = Some(if resolver_timeout {
            BROWSER_TIMEOUT_MS
        } else {
            self.request.timeout_ms()
        });
        check.duration_ms = Some(step.duration_ms);
        let error_text = step.detail(connect_stage::ERROR);
        let no_addresses = match step.stage {
            Stage::Dns => error_text.is_none() && step.os_error().is_none(),
            _ => step.detail(connect_stage::RESULT) == Some("noAddresses"),
        };
        let (code, from_text, message) = if step.abandoned() {
            (
                "timedOut",
                false,
                format!("no answer within {} s", self.request.timeout().as_secs()),
            )
        } else if resolver_timeout {
            (
                "timedOut",
                false,
                error_text.map_or_else(
                    || format!("no answer within {BROWSER_TIMEOUT_MS} ms"),
                    |text| text.trim_end_matches('.').to_string(),
                ),
            )
        } else if no_addresses {
            (
                "noAddresses",
                false,
                "the name resolved to no address".to_string(),
            )
        } else {
            let text = error_text.unwrap_or_default();
            let (code, from_text) = dns_result_from(step.os_error(), text);
            (code, from_text, text.to_string())
        };
        check.fields.push(Field::plain("result", code));
        if from_cache {
            check
                .fields
                .push(Field::plain("source", connect_stage::SOURCE_FALLBACK_CACHE));
            for address in &addresses {
                check.fields.push(Field::identifier(
                    "addresses",
                    address.clone(),
                    Category::Address,
                ));
            }
        }
        // Read from the resolver's message, not its code: weaker evidence.
        check.coverage = match code {
            "notFound" | "noAddresses" if from_text => Coverage::Classified,
            "notFound" | "noAddresses" => Coverage::Diagnosed,
            _ => Coverage::Inconclusive,
        };
        check.domain = Some(if check.coverage != Coverage::Inconclusive {
            Domain::specific("connectivity.nameResolution")
        } else {
            Domain::undetermined(&["connectivity.nameResolution", "connectivity.network"])
        });
        self.checks.push(check);
        self.quoting_finding(
            Certainty::Confirmed,
            id,
            format!("Name resolution for {host} failed ({code}): {message}."),
        );
        let mut record = ErrorRecord::new("os", id, message);
        record.os_error = step.os_error();
        record.code = Some(code.to_string());
        record.from_message_text = from_text;
        record.superseded = superseded;
        self.errors.push(record);
        if from_cache {
            self.finding(
                Certainty::Informational,
                id,
                format!(
                    "The client went on with addresses for {host} cached from an earlier resolution; the TCP connect is checked with them."
                ),
            );
        }
        if from_text {
            self.limitation(
                "This platform does not expose the resolver's result code; the name-resolution \
                 result was classified from the resolver's message.",
            );
        }
        // The cached addresses are still connected to: nothing is blocked.
        !from_cache
    }

    /// A local named instance without a protocol prefix, at a depth that
    /// stops after resolution: the client would try shared memory first and
    /// ask SQL Server Browser only if that failed, so the instance was not
    /// looked up and nothing about it is known.
    fn instance_not_looked_up(&mut self, target: &Target) {
        let id = CheckId::InstanceResolution;
        let mut check = Check::new(id, Coverage::Inconclusive);
        check.fields.push(Field::identifier(
            "host",
            target.resolvable_host().into_owned(),
            Category::Host,
        ));
        check.fields.push(Field::identifier(
            "instance",
            target.instance.clone().unwrap_or_default(),
            Category::Instance,
        ));
        check.fields.push(Field::plain("result", "notLookedUp"));
        check.domain = Some(Domain::undetermined(&["connectivity.instanceResolution"]));
        self.checks.push(check);
        self.finding(
            Certainty::Informational,
            id,
            "Without a protocol prefix sqlcmd connects to a local named instance over shared \
             memory first and asks SQL Server Browser only if that fails; this depth opens no \
             connection, so the instance was not looked up. Run the default depth, or use \
             tcp:host\\instance to check the SQL Server Browser lookup.",
        );
    }

    /// Instance resolution, from the `instance_lookup` stage. What it blocks
    /// when it gave no endpoint.
    fn instance_resolution(
        &mut self,
        target: &Target,
        lookup: Option<&Step>,
        over_tcp: bool,
        not_tcp: Option<&'static str>,
        pipe_used: Option<&Step>,
        timed_out: bool,
    ) -> Option<Skip> {
        let id = CheckId::InstanceResolution;
        let Some(step) = lookup else {
            let reason = match not_tcp {
                Some(reason) if !over_tcp => reason,
                _ if target.protocol == Protocol::Admin => {
                    "the dedicated administrator connection uses port 1434"
                }
                _ if target.instance.is_some() && target.port.is_some() => "a port was given",
                _ => "not a named instance",
            };
            self.checks
                .push(Check::skipped(id, Skip::NotApplicable(reason)));
            return None;
        };
        let host = step
            .detail(connect_stage::SERVER)
            .map_or_else(|| target.resolvable_host().into_owned(), str::to_string);
        let instance = step
            .detail(connect_stage::INSTANCE)
            .map(str::to_string)
            .or_else(|| target.instance.clone())
            .unwrap_or_default();
        let mut check = Check::new(id, Coverage::Passed);
        check.deadline_ms = Some(BROWSER_TIMEOUT_MS);
        check.duration_ms = Some(step.duration_ms);
        check
            .fields
            .push(Field::identifier("host", host.clone(), Category::Host));
        check.fields.push(Field::identifier(
            "instance",
            instance.clone(),
            Category::Instance,
        ));
        // An explicit TCP target needs a TCP port: an answer that lists only
        // a pipe gives it none.
        let no_tcp_port = step.ok == Some(true)
            && target.protocol == Protocol::Tcp
            && step.detail(connect_stage::PORT).is_none();
        if step.ok == Some(true) && !no_tcp_port {
            if let Some(port) = step.detail(connect_stage::PORT) {
                check.fields.push(Field::plain("port", port));
            }
            // The pipe SQL Server Browser listed or, for np: without one,
            // the instance's standard pipe the client then opened.
            let pipe = match (step.detail(connect_stage::PIPE_PATH), pipe_used) {
                (Some(pipe), _) => Some((pipe, "sqlBrowser")),
                (None, Some(used)) if target.protocol == Protocol::NamedPipe => used
                    .detail(connect_stage::PIPE_PATH)
                    .map(|pipe| (pipe, "standard")),
                _ => None,
            };
            if let Some((pipe, source)) = pipe {
                check
                    .fields
                    .push(Field::identifier("pipe", pipe, Category::Pipe));
                check.fields.push(Field::plain("pipeSource", source));
            }
            self.checks.push(check);
            return None;
        }
        if step.abandoned() && !timed_out {
            // A cancel cut the lookup short: it did not fail.
            self.checks.push(Check::skipped(id, Skip::Canceled));
            return Some(Skip::Canceled);
        }
        if lookup_name_failure(step) {
            // Name resolution reports it.
            self.checks
                .push(Check::skipped(id, Skip::BlockedBy(CheckId::NameResolution)));
            return Some(Skip::BlockedBy(CheckId::NameResolution));
        }
        // What is blocked without the endpoint: a port, which only the user
        // can then give, or a pipe.
        let needs_port = target.protocol.uses_tcp();
        let blocks = if needs_port {
            Skip::PortRequired
        } else {
            Skip::BlockedBy(id)
        };
        let code = match step.detail(connect_stage::RESULT) {
            _ if no_tcp_port => "noTcpPort",
            _ if step.abandoned() && timed_out => "timedOut",
            Some(connect_stage::NO_ENDPOINT) => "noTcpPort",
            Some(code) => code,
            None => "failed",
        };
        let os_error = step.os_error();
        let problem = ssrp_problem(code, step.detail(connect_stage::ERROR));
        let mut record = ErrorRecord::new("sqlBrowser", id, problem.clone());
        record.code = Some(code.to_string());
        record.os_error = os_error;
        check.fields.push(Field::plain("result", code));
        if needs_port {
            check
                .fields
                .push(Field::plain(REQUIRES, REQUIRES_EXPLICIT_PORT));
        }
        if ssrp_local_failure(code, os_error) {
            // This computer would not let the request out: the check says
            // nothing about the Browser, so the diagnosis is partial, as
            // for a TCP connect this computer refuses.
            check.coverage = Coverage::Inconclusive;
            check.domain = Some(Domain::specific("client.platform"));
            self.checks.push(check);
            self.errors.push(record);
            self.status = ExecutionStatus::Partial;
            self.limitation(&format!(
                "This computer did not let sqlcmd ask SQL Server Browser for the instance's \
                 port ({problem}); instance resolution could not be checked."
            ));
            if needs_port {
                self.explicit_port_request(&host);
            }
            return Some(blocks);
        }
        // An answer that could not be used is evidence about the Browser; no
        // answer at all is not.
        check.coverage = match code {
            "invalidResponse" | "noTcpPort" => Coverage::Diagnosed,
            _ => Coverage::Inconclusive,
        };
        check.domain = Some(if check.coverage == Coverage::Diagnosed {
            Domain::specific("connectivity.instanceResolution")
        } else {
            Domain::undetermined(&["connectivity.instanceResolution", "connectivity.network"])
        });
        self.checks.push(check);
        self.quoting_finding(
            Certainty::Confirmed,
            id,
            format!(
                "SQL Server Browser on {host} gave no usable {} for instance {instance}: \
                 {problem}.",
                if target.protocol.uses_tcp() {
                    "port"
                } else {
                    "pipe"
                }
            ),
        );
        if needs_port {
            self.explicit_port_request(&host);
        }
        self.errors.push(record);
        Some(blocks)
    }

    /// The request for an explicit `host,port`, made when SQL Server
    /// Browser gave a named instance no usable port, whatever the depth: no
    /// port is guessed, so only the user can give one.
    fn explicit_port_request(&mut self, host: &str) {
        self.finding(
            Certainty::Informational,
            CheckId::InstanceResolution,
            format!(
                "Give the instance's port explicitly, as -S {host},<port>: sqlcmd does not try \
                 port 1433 or any other port for a named instance whose port SQL Server \
                 Browser did not give."
            ),
        );
    }

    /// The TCP connects of the TCP path, the addresses after the one used
    /// included. What it blocks when no address accepted.
    fn tcp_connect(
        &mut self,
        target: &Target,
        tcp: Option<&Transport>,
        blocked: Option<Skip>,
        not_tcp: Option<&'static str>,
        superseded: bool,
        canceled: bool,
    ) -> Option<Skip> {
        let id = CheckId::TcpConnect;
        let Some(tcp) = tcp else {
            match blocked {
                Some(skip) if target.protocol.uses_tcp() => {
                    self.checks.push(Check::skipped(id, skip.clone()));
                    return Some(skip);
                }
                _ => {
                    let reason = not_tcp.unwrap_or(NOT_TCP);
                    self.checks
                        .push(Check::skipped(id, Skip::NotApplicable(reason)));
                    return None;
                }
            }
        };
        if let Some(dns) = tcp.dns
            && dns.ok != Some(true)
        {
            let skip = Skip::BlockedBy(CheckId::NameResolution);
            self.checks.push(Check::skipped(id, skip.clone()));
            return Some(skip);
        }
        // A cancel cut the connects short: the ones it abandoned did not
        // fail, and without one that connected the check did not finish.
        let steps: Vec<&Step> = tcp
            .tcp
            .iter()
            .copied()
            .filter(|s| !canceled || s.ok.is_some())
            .collect();
        if canceled && !steps.iter().any(|s| s.ok == Some(true)) {
            let listed = tcp.dns.map_or(0, |dns| listed_addresses(dns).len());
            if steps.len() < tcp.tcp.len() || steps.is_empty() || steps.len() < listed {
                self.checks.push(Check::skipped(id, Skip::Canceled));
                return Some(Skip::Canceled);
            }
        }
        let budget_s = self.request.timeout().as_secs();
        let port: u16 = tcp
            .tcp
            .iter()
            .find_map(|s| s.detail(connect_stage::PORT)?.parse().ok())
            .or(target.port)
            .unwrap_or(if target.protocol == Protocol::Admin {
                DAC_PORT
            } else {
                1433
            });
        let mut check = Check::new(id, Coverage::Passed);
        check.deadline_ms = Some(self.request.timeout_ms());
        check.fields.push(Field::plain("port", port.to_string()));
        let mut records = Vec::new();
        for step in &steps {
            let address = step
                .detail(connect_stage::ADDRESS)
                .unwrap_or_default()
                .to_string();
            let port: u16 = step
                .detail(connect_stage::PORT)
                .and_then(|p| p.parse().ok())
                .unwrap_or(port);
            let (result, os_error, message) = match step.ok {
                Some(true) => (TcpResult::Connected, None, None),
                Some(false) if step.detail(connect_stage::TIMED_OUT) == Some("true") => (
                    TcpResult::TimedOut,
                    None,
                    Some(format!("no answer within {} ms", self.request.timeout_ms())),
                ),
                Some(false) => {
                    let result = TcpResult::from_fields(
                        step.detail(connect_stage::IO_KIND),
                        step.os_error(),
                    );
                    let message = step
                        .detail(connect_stage::ERROR)
                        .map_or_else(|| result.name().to_string(), str::to_string);
                    (result, step.os_error(), Some(message))
                }
                // Abandoned: the attempt's time limit dropped it before it
                // answered. It did not fail; it did not answer in time.
                None => (
                    TcpResult::TimedOut,
                    None,
                    Some(format!("no answer within the {budget_s} s time limit")),
                ),
            };
            check.attempts.push(TcpAttempt {
                address: address.clone(),
                port,
                result,
                os_error,
                duration_ms: step.duration_ms,
            });
            if let Some(message) = message {
                let mut record = ErrorRecord::new("os", id, format!("{address}:{port}: {message}"));
                record.os_error = os_error;
                record.code = Some(result.name().to_string());
                record.superseded = superseded;
                records.push(record);
            }
        }
        let connected = check
            .attempts
            .iter()
            .any(|a| a.result == TcpResult::Connected);
        if !connected && let Some(dns) = tcp.dns {
            // The time limit was spent before these were tried.
            for address in listed_addresses(dns) {
                if check.attempts.iter().any(|a| a.address == address) {
                    continue;
                }
                check.attempts.push(TcpAttempt {
                    address: address.clone(),
                    port,
                    result: TcpResult::NotAttempted,
                    os_error: None,
                    duration_ms: 0,
                });
                let mut record = ErrorRecord::new(
                    "os",
                    id,
                    format!(
                        "{address}:{port}: not tried: the {budget_s} s time limit was spent on \
                         earlier addresses"
                    ),
                );
                record.code = Some(TcpResult::NotAttempted.name().to_string());
                record.superseded = superseded;
                records.push(record);
            }
        }
        let first = steps.iter().map(|s| s.started).min();
        let last = steps.iter().map(|s| s.ended).max();
        check.duration_ms = Some(match (first, last) {
            (Some(first), Some(last)) => {
                u64::try_from(last.saturating_duration_since(first).as_millis()).unwrap_or(u64::MAX)
            }
            _ => 0,
        });
        self.errors.extend(records);

        if connected {
            self.checks.push(check);
            return None;
        }
        let blocks = Some(Skip::BlockedBy(CheckId::TcpConnect));
        // The addresses a local failure left untried say nothing either way:
        // the ones tried must all have failed here, and at least one was.
        let tried: Vec<&TcpAttempt> = check
            .attempts
            .iter()
            .filter(|a| a.result != TcpResult::NotAttempted)
            .collect();
        // Port 0, which the client takes as a port, is no port a server
        // listens on: the operating system refuses every connect to it (as
        // an address not valid here, 10049 on Windows, or as a refusal), so
        // the port given is wrong, not this computer or the network.
        if port == 0 && !tried.is_empty() {
            check.coverage = Coverage::Diagnosed;
            check.domain = Some(Domain::specific("client.inputConfiguration"));
            self.checks.push(check);
            self.finding(
                Certainty::Confirmed,
                id,
                "The port is 0, which no server listens on: no TCP connection to it can be \
                 made."
                    .to_string(),
            );
            return blocks;
        }
        if !tried.is_empty()
            && tried
                .iter()
                .all(|a| a.os_error.is_some_and(local_facility_error))
        {
            // This computer refused to open the connection: the check says
            // nothing about the target, so the diagnosis is partial.
            let codes: Vec<String> = tried
                .iter()
                .filter_map(|a| a.os_error.map(|e| e.to_string()))
                .collect();
            check.coverage = Coverage::Inconclusive;
            check.domain = Some(Domain::specific("client.platform"));
            self.checks.push(check);
            self.status = ExecutionStatus::Partial;
            self.limitation(&format!(
                "This computer did not let sqlcmd open a network connection (operating-system \
                 error {}); network reachability could not be checked.",
                codes.join(", ")
            ));
            return blocks;
        }

        let results: Vec<TcpResult> = check.attempts.iter().map(|a| a.result).collect();
        let all = |r: TcpResult| !results.is_empty() && results.iter().all(|x| *x == r);
        // No answer, or no time left to ask: nothing says whether the port is open.
        let unanswered = results
            .iter()
            .all(|r| matches!(r, TcpResult::TimedOut | TcpResult::NotAttempted));
        check.coverage = if unanswered {
            Coverage::Inconclusive
        } else {
            Coverage::Diagnosed
        };
        check.domain = Some(if all(TcpResult::Refused) {
            Domain::undetermined(&["connectivity.network", "target.sqlServer"])
        } else if !results.is_empty()
            && results.iter().all(|r| {
                matches!(
                    r,
                    TcpResult::HostUnreachable | TcpResult::NetworkUnreachable
                )
            })
        {
            Domain::specific("connectivity.network")
        } else {
            Domain::undetermined(&["connectivity.network"])
        });
        let summary: Vec<String> = check
            .attempts
            .iter()
            .map(|a| format!("{} {}", a.address, a.result.name()))
            .collect();
        self.checks.push(check);
        self.finding(
            Certainty::Confirmed,
            id,
            format!(
                "No address accepted a TCP connection on port {port}: {}.",
                summary.join(", ")
            ),
        );
        if all(TcpResult::Refused) {
            self.finding(
                Certainty::Suspected,
                id,
                format!("Nothing is listening on port {port} at those addresses."),
            );
        }
        blocks
    }

    /// SQL Server Browser gave the port, but the attempt ended (its time
    /// limit spent on the lookup, say) before any TCP connect: TCP was
    /// needed, so the check is not attempted, with what ended the attempt,
    /// rather than not applicable. What it blocks.
    fn tcp_not_reached(&mut self, port: Option<&str>, ended: &Ended) -> Option<Skip> {
        let id = CheckId::TcpConnect;
        let error = match ended {
            Ended::Failed(error) => error,
            Ended::Canceled => {
                self.checks.push(Check::skipped(id, Skip::Canceled));
                return Some(Skip::Canceled);
            }
            // An attempt that did not fail ended on a transport.
            Ended::Connected | Ended::Stopped => {
                self.checks
                    .push(Check::skipped(id, Skip::NotApplicable(NOT_TCP)));
                return None;
            }
        };
        let classified = classify_connection_error(error, None, &self.request.authentication);
        let mut check = Check::new(id, Coverage::Inconclusive);
        check.deadline_ms = Some(self.request.timeout_ms());
        if let Some(port) = port {
            check.fields.push(Field::plain("port", port));
        }
        check
            .fields
            .push(Field::plain("result", TcpResult::NotAttempted.name()));
        check.domain = Some(classified.domain);
        self.checks.push(check);
        self.quoting_finding(
            Certainty::Confirmed,
            id,
            format!(
                "No TCP connection to port {} was attempted: the connection attempt ended \
                 first: {error}.",
                port.unwrap_or_default()
            ),
        );
        self.errors
            .extend(classified.errors.into_iter().map(|mut record| {
                record.check = id;
                record
            }));
        Some(Skip::BlockedBy(id))
    }

    /// The pipe the attempt ended on (`np:`, `lpc:`, LocalDB) did not open:
    /// reachability is checked on that transport, which the check names, and
    /// not on TCP. What it blocks.
    fn pipe_not_opened(&mut self, transport: &Transport, ended: &Ended) -> Option<Skip> {
        let id = CheckId::TcpConnect;
        let pipe = transport.pipe?;
        if pipe.abandoned() && ended.canceled() {
            self.checks.push(Check::skipped(id, Skip::Canceled));
            return Some(Skip::Canceled);
        }
        let protocol = transport.protocol();
        // The pipe ends the run here: its errors are worded as this
        // transport's, as the connection attempt words its own.
        self.transport = Some(protocol);
        // No answer in time: the attempt's limit abandoned the open, or the
        // open ended on the pipe's own time limit for a free instance, which
        // the client records as `TimedOut` (as it does Windows' semaphore
        // time-out, error 121).
        let timed_out = pipe.abandoned() || pipe.detail(connect_stage::IO_KIND) == Some("TimedOut");
        let message = if pipe.abandoned() {
            format!(
                "no answer within the {} s time limit",
                self.request.timeout().as_secs()
            )
        } else {
            pipe.detail(connect_stage::ERROR)
                .map(str::to_string)
                .or_else(|| match ended {
                    Ended::Failed(error) => Some(error.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "it was not opened".to_string())
        };
        let code = if timed_out { "timedOut" } else { "failed" };
        // The operating system said no, as for a refused TCP connect; no
        // answer in time says nothing.
        let mut check = Check::new(
            id,
            if pipe.failed() && !timed_out {
                Coverage::Diagnosed
            } else {
                Coverage::Inconclusive
            },
        );
        check.deadline_ms = Some(self.request.timeout_ms());
        check.duration_ms = Some(pipe.duration_ms);
        check
            .fields
            .push(Field::plain("transport", protocol.name()));
        if let Some(path) = pipe.detail(connect_stage::PIPE_PATH) {
            check
                .fields
                .push(Field::identifier("pipe", path, Category::Pipe));
        }
        check.fields.push(Field::plain("result", code));
        check.domain = Some(Domain::undetermined(&[
            "connectivity.network",
            "target.sqlServer",
        ]));
        self.checks.push(check);
        let what = if protocol == Protocol::SharedMemory {
            "Shared memory to this computer".to_string()
        } else {
            format!("The {} transport", protocol.name())
        };
        self.quoting_finding(
            Certainty::Confirmed,
            id,
            format!("{what} did not open ({message})."),
        );
        let mut record = ErrorRecord::new("os", id, message);
        record.os_error = pipe.os_error();
        record.code = Some(code.to_string());
        self.errors.push(record);
        Some(Skip::BlockedBy(id))
    }

    /// The TCP path's checks, when the connection used another transport:
    /// what failed there is not on its path. Each failed or skipped check
    /// does not apply, for `reason`, and its findings go; the errors stay as
    /// evidence, marked superseded, as a normal run would not print them.
    fn not_on_the_path(&mut self, reason: &'static str) {
        const PATH: [CheckId; 3] = [
            CheckId::NameResolution,
            CheckId::InstanceResolution,
            CheckId::TcpConnect,
        ];
        let mut set_aside = Vec::new();
        for check in &mut self.checks {
            if PATH.contains(&check.id)
                && (check.coverage.failed() || check.coverage == Coverage::Skipped)
            {
                *check = Check::skipped(check.id, Skip::NotApplicable(reason));
                set_aside.push(check.id);
            }
        }
        for error in &mut self.errors {
            if PATH.contains(&error.check) {
                error.superseded = true;
            }
        }
        self.findings.retain(|f| !set_aside.contains(&f.check));
    }

    /// The transports the client's protocol order moved past (shared memory
    /// that did not open, a TCP path that failed): an informational finding
    /// each, and their errors kept as evidence, marked superseded, as a
    /// normal run reports only the last transport's.
    fn moved_past(&mut self, groups: &[Transport], current: Option<usize>, all: &[&Step]) {
        for (index, group) in groups.iter().enumerate() {
            if Some(index) == current {
                continue;
            }
            // Without a next transport, SQL Server Browser was asked next,
            // for the TCP port.
            let next = groups.get(index + 1);
            let next_name = match next.map(Transport::protocol) {
                Some(Protocol::Tcp) | None => "TCP",
                Some(Protocol::SharedMemory) => "shared memory",
                Some(_) => "named pipes",
            };
            match group.pipe {
                Some(pipe) => {
                    let reason = pipe
                        .detail(connect_stage::ERROR)
                        .unwrap_or("it was not opened")
                        .to_string();
                    let what = if group.protocol() == Protocol::SharedMemory {
                        "Shared memory to this computer".to_string()
                    } else {
                        format!("The {} transport", group.protocol().name())
                    };
                    self.quoting_finding(
                        Certainty::Informational,
                        CheckId::ConnectionAttempt,
                        format!(
                            "{what} did not open ({reason}); without a protocol prefix sqlcmd \
                             then tries {next_name}, checked below."
                        ),
                    );
                    let mut record =
                        ErrorRecord::new("os", CheckId::ConnectionAttempt, reason.clone());
                    record.os_error = pipe.os_error();
                    record.superseded = true;
                    self.errors.push(record);
                }
                None if !group.opened() => self.finding(
                    Certainty::Informational,
                    CheckId::ConnectionAttempt,
                    if self.wants(Depth::ConnectionAttempt) {
                        "TCP did not connect; without a protocol prefix sqlcmd then tries named \
                         pipes, so the connection attempt was made over the server's standard \
                         pipe."
                    } else {
                        "TCP did not connect; without a protocol prefix sqlcmd then tries the \
                         server's standard pipe."
                    },
                ),
                None => {
                    // TCP opened, and the attempt over it failed in a way
                    // the client moves on from.
                    let from = group.started();
                    let until = next.and_then(Transport::started);
                    let failed: Vec<&Step> = all
                        .iter()
                        .copied()
                        .filter(|s| {
                            matches!(s.stage, Stage::Prelogin | Stage::Tls | Stage::Login)
                                && s.failed()
                                && from.is_none_or(|f| s.started >= f)
                                && until.is_none_or(|u| s.started < u)
                        })
                        .collect();
                    for step in failed {
                        let mut record = ErrorRecord::new(
                            "client",
                            CheckId::ConnectionAttempt,
                            step.detail(connect_stage::ERROR)
                                .unwrap_or(step.stage.title())
                                .to_string(),
                        );
                        record.os_error = step.os_error();
                        record.superseded = true;
                        self.errors.push(record);
                    }
                    self.finding(
                        Certainty::Informational,
                        CheckId::ConnectionAttempt,
                        "The connection attempt over TCP failed; without a protocol prefix \
                         sqlcmd then tries named pipes, so the connection attempt was made over \
                         the server's standard pipe.",
                    );
                }
            }
        }
    }

    fn connection_input(&mut self) -> Option<Target> {
        let started = Instant::now();
        let parsed = target::parse_for(&self.request.server, self.windows);
        let mut check = Check::new(CheckId::ConnectionInput, Coverage::Passed);
        check.duration_ms = Some(millis(started));
        match parsed {
            Ok(target) => {
                check
                    .fields
                    .push(Field::plain("protocol", target.protocol_name(self.windows)));
                check.fields.push(Field::identifier(
                    "host",
                    target.host.clone(),
                    Category::Host,
                ));
                if let Some(instance) = &target.instance {
                    check.fields.push(Field::identifier(
                        "instance",
                        instance.clone(),
                        Category::Instance,
                    ));
                }
                if let Some(port) = target.port {
                    check.fields.push(Field::plain("port", port.to_string()));
                }
                if let Some(database) = self.request.database.as_ref().filter(|d| !d.is_empty()) {
                    check.fields.push(Field::identifier(
                        "database",
                        database.clone(),
                        Category::Database,
                    ));
                }
                self.checks.push(check);
                self.target = Some(target.clone());
                if target.instance.is_some() && target.port.is_some() {
                    self.finding(
                        Certainty::Informational,
                        CheckId::ConnectionInput,
                        "The server includes both an instance name and a port; sqlcmd uses the port and does not look up the instance.",
                    );
                }
                if self.request.trust_server_certificate && self.request.encrypt == Encrypt::Strict
                {
                    self.finding(
                        Certainty::Informational,
                        CheckId::ConnectionInput,
                        "-C is ignored with strict encryption (-Ns): the server certificate is validated.",
                    );
                } else if self.request.trust_server_certificate
                    && self
                        .request
                        .host_name_in_certificate
                        .as_deref()
                        .is_some_and(|name| !name.is_empty())
                {
                    self.finding(
                        Certainty::Informational,
                        CheckId::ConnectionInput,
                        "The certificate host name from -F is not validated when -C trusts the server certificate.",
                    );
                }
                Some(target)
            }
            Err(error) => {
                check.coverage = Coverage::Diagnosed;
                check.fields.push(Field::plain("error", error.code()));
                check.domain = Some(Domain::specific("client.inputConfiguration"));
                self.checks.push(check);
                // The client's own messages end with their period.
                let reason = error.to_string();
                let reason = reason.strip_suffix('.').unwrap_or(&reason);
                self.finding(
                    Certainty::Confirmed,
                    CheckId::ConnectionInput,
                    format!("The server given to -S could not be parsed: {reason}."),
                );
                let mut record =
                    ErrorRecord::new("sqlcmd", CheckId::ConnectionInput, error.to_string());
                record.code = Some(error.code().to_string());
                self.errors.push(record);
                self.skip_rest(
                    CheckId::ConnectionInput,
                    &Skip::BlockedBy(CheckId::ConnectionInput),
                );
                None
            }
        }
    }

    fn client_context(&self) -> ClientContext {
        let request = &self.request;
        let mut context = ClientContext::default();
        // sqlcmd connects without MultiSubnetFailover unless -M, and diagnose
        // reports each address on its own, which the parallel connect does not.
        context.multi_subnet_failover = Some(false);
        match &request.authentication {
            Authentication::SqlPassword { user, password } => {
                context.tds_authentication_method = TdsAuthenticationMethod::Password;
                context.user_name = user.clone();
                context.password = password.clone();
            }
            Authentication::Integrated => {
                context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
            }
        }
        if let Some(database) = &request.database {
            context.database = database.clone();
        }
        context.encryption_options = EncryptionOptions {
            mode: match request.encrypt {
                Encrypt::Optional => EncryptionSetting::PreferOff,
                Encrypt::Mandatory => EncryptionSetting::On,
                Encrypt::Strict => EncryptionSetting::Strict,
            },
            trust_server_certificate: request.trust_server_certificate,
            host_name_in_cert: request.host_name_in_certificate.clone(),
            server_certificate: None,
        };
        let seconds = u32::try_from(request.timeout().as_secs()).unwrap_or(u32::MAX);
        context.connect_timeout = seconds;
        context.login_timeout = Some(seconds);
        context.ssrp_timeout_ms = BROWSER_TIMEOUT_MS;
        // One connection: no retry.
        context.connect_retry_count = 0;
        context
    }

    /// The connection attempt: the pre-login, TLS and login phases on the
    /// transport the attempt ended on (`last`), and how it `ended`.
    #[allow(clippy::too_many_arguments)]
    async fn connection_attempt(
        &mut self,
        target: &Target,
        all: &[&Step],
        last: Option<&Transport<'_>>,
        redirected: bool,
        ended: &Ended,
        attempt_ms: u64,
        client: Option<&TdsClient>,
    ) {
        let mut check = Check::new(CheckId::ConnectionAttempt, Coverage::Passed);
        check.duration_ms = Some(attempt_ms);
        check.deadline_ms = Some(self.request.timeout_ms());
        // The phases on the last transport: after a routing redirect, those
        // of the connection the client made where it was sent; after a
        // transport the protocol order moved past, the next one's.
        let from = last.and_then(Transport::started);
        let mut phases: Vec<Step> = all
            .iter()
            .filter(|s| matches!(s.stage, Stage::Prelogin | Stage::Tls | Stage::Login))
            .filter(|s| from.is_none_or(|f| s.started >= f))
            .map(|s| (*s).clone())
            .collect();
        // In the order they ended: login, which contains the TLS handshake
        // when encryption starts at login, follows it.
        phases.sort_by_key(|s| s.ended);
        let redirected = followed_redirect(&mut phases) || redirected;
        if matches!(ended, Ended::Failed(_)) {
            drop_enclosing_failures(&mut phases);
        }
        check.subphases = phases;
        if redirected {
            check.fields.push(Field::plain("redirected", "true"));
        }
        // Without a prefix, the protocol order may end on a pipe.
        if let Some(last) = last
            && target.protocol == Protocol::Default
            && last.protocol() != Protocol::Tcp
        {
            let transport = last.protocol();
            check
                .fields
                .push(Field::plain("transport", transport.name()));
            if transport == Protocol::NamedPipe
                && let Some(pipe) = last.pipe.and_then(|p| p.detail(connect_stage::PIPE_PATH))
            {
                check
                    .fields
                    .push(Field::identifier("pipe", pipe, Category::Pipe));
            }
            self.transport = Some(transport);
        }
        self.spn = all
            .iter()
            .rev()
            .filter(|s| s.stage == Stage::Login)
            .find_map(|s| s.detail(connect_stage::SPN))
            .map(str::to_string);
        self.limitation(
            "The connection attempt is made with the mssql-tds client, which reports its \
             pre-login, TLS and login phases; it is not the ODBC driver sqlcmd uses for queries, \
             so TLS and login behavior can differ from a query run.",
        );
        let cache_location = if self.request.authentication == Authentication::Integrated {
            self.kerberos_evidence(&mut check, self.request.timeout())
                .await
        } else {
            None
        };

        match (ended, client) {
            (Ended::Failed(error), _) => {
                let failed_phase = failed_stage(&check.subphases, ended.timed_out());
                let classified =
                    classify_connection_error(error, failed_phase, &self.request.authentication);
                check.coverage = classified.coverage;
                check.domain = Some(classified.domain);
                if let Some(phase) = failed_phase {
                    check.fields.push(Field::plain("failedPhase", phase.name()));
                }
                if let Some(category) = classified.tls_category {
                    check.fields.push(Field::plain("tlsFailure", category));
                }
                let no_ticket_cache = check.field("kerberosTicketCache") == Some("notFound");
                self.checks.push(check);
                if no_ticket_cache && failed_phase == Some(Stage::Login) {
                    self.finding(
                        Certainty::Suspected,
                        CheckId::ConnectionAttempt,
                        no_ticket_cache_finding(cache_location.as_deref()),
                    );
                }
                self.authentication = Some(classified.auth);
                self.quoting_finding(
                    Certainty::Confirmed,
                    CheckId::ConnectionAttempt,
                    classified.finding,
                );
                self.errors.extend(classified.errors);
                if classified.from_message_text {
                    self.limitation(
                        "The TLS failure was classified from the error text; no stable error \
                         code was available.",
                    );
                }
            }
            (_, Some(client)) => {
                if let Some(encryption) = check
                    .subphases
                    .iter()
                    .find(|s| s.stage == Stage::Prelogin)
                    .and_then(|s| s.detail(connect_stage::ENCRYPTION))
                {
                    let encryption = encryption_name(encryption).to_string();
                    check.fields.push(Field::plain("encryption", encryption));
                }
                self.checks.push(check);
                self.authentication = Some(AuthOutcome::Succeeded);
                self.server = Some(ServerInfo {
                    name: client.server_reported_name().map(str::to_string),
                    version: client.server_version().map(version_text),
                    database: client.database().to_string(),
                    encrypted: client.is_encrypted(),
                    engine_edition: None,
                });
            }
            // Stopped: below this depth, so not reached.
            (_, None) => self.checks.push(check),
        }
    }

    /// The client's side of Kerberos/SSPI: the SPN the attempt requested and,
    /// off Windows, whether a ticket cache exists. Windows keeps tickets in
    /// the logon session, which needs no cache. The cache may be on a slow
    /// (network) path, so it is looked for on a thread of its own within
    /// `budget`, what is left of the attempt's time limit. Where it looked
    /// (`KRB5CCNAME`, `default`, or the cache type), when it did.
    async fn kerberos_evidence(&mut self, check: &mut Check, budget: Duration) -> Option<String> {
        if let Some(spn) = self.spn.clone() {
            check
                .fields
                .push(Field::identifier("spn", spn, Category::Spn));
        }
        let mut looked_in = None;
        let found = match self.ticket_cache {
            Some(lookup) => Some(within(budget, lookup).await),
            None => None,
        };
        match found {
            None => {
                check
                    .fields
                    .push(Field::plain("kerberosTicketCache", "notApplicable"));
            }
            Some(Some((state, location))) => {
                check
                    .fields
                    .push(Field::plain("kerberosTicketCache", state));
                if state == "unverified" {
                    self.limitation(&format!(
                        "The Kerberos ticket cache is a {location} cache, which sqlcmd does not \
                         inspect."
                    ));
                } else if location == "default" {
                    self.limitation(
                        "Only the default file ticket cache was looked for; a KCM or KEYRING \
                         cache set in krb5.conf is not inspected.",
                    );
                }
                looked_in = Some(location);
            }
            Some(None) => {
                check
                    .fields
                    .push(Field::plain("kerberosTicketCache", "notChecked"));
                self.limitation(
                    "The Kerberos ticket cache could not be looked for within the time limit.",
                );
            }
        }
        self.limitation(
            "Kerberos encryption types and delegation are not inspected; the SPN is the one the \
             client requests, not proof that it is registered.",
        );
        looked_in
    }
    async fn session_validation(&mut self, client: &mut TdsClient) {
        let mut check = Check::new(CheckId::SessionValidation, Coverage::Passed);
        check.deadline_ms = Some(self.request.timeout_ms());
        let sent_unix_ms = unix_ms_now();
        let started = Instant::now();
        let result = tokio::time::timeout(self.request.timeout(), session_probe(client)).await;
        check.duration_ms = Some(millis(started));
        let received_unix_ms = unix_ms_now();
        let error = match result {
            Ok(Ok(probe)) => {
                self.session_probed(check, &probe, sent_unix_ms, received_unix_ms);
                return;
            }
            Ok(Err(error)) => Some(error),
            Err(_) => None,
        };
        self.session_failed(check, error);
    }

    /// Session validation passed: what the minimal query (`probe`) read, the
    /// query sent and answered at `sent_unix_ms` and `received_unix_ms` on
    /// the client's clock.
    fn session_probed(
        &mut self,
        mut check: Check,
        probe: &SessionProbe,
        sent_unix_ms: i64,
        received_unix_ms: i64,
    ) {
        check.fields.push(Field::plain(
            "engineEdition",
            probe.engine_edition.to_string(),
        ));
        if let Some(server) = &mut self.server {
            server.engine_edition = Some(probe.engine_edition);
            if let Some(target_type) = server.target_type() {
                check.fields.push(Field::plain("targetType", target_type));
            }
        }
        if let Some(server_ms) = probe.server_unix_ms {
            // The server read its clock while the query was in flight:
            // compare it with the middle of the round trip.
            let client_ms = sent_unix_ms + (received_unix_ms - sent_unix_ms) / 2;
            let offset = server_ms - client_ms;
            check
                .fields
                .push(Field::plain("clockOffsetMs", offset.to_string()));
            if offset.abs() > KERBEROS_CLOCK_TOLERANCE_MS {
                self.finding(
                    Certainty::Informational,
                    CheckId::SessionValidation,
                    format!(
                        "The server's clock is {} s {} the client's, more than the 5 \
                         minutes Kerberos tolerates by default.",
                        offset.abs() / 1000,
                        if offset > 0 { "ahead of" } else { "behind" }
                    ),
                );
            }
        }
        self.checks.push(check);
    }

    /// Session validation failed: the minimal query's `error`, or no answer
    /// in the time limit (`None`).
    fn session_failed(&mut self, mut check: Check, error: Option<Error>) {
        let id = CheckId::SessionValidation;
        // The error's text is the client's or the server's: the finding
        // quoting it is flagged for review. The time-limit text is sqlcmd's.
        let (finding, quotes) = match error {
            Some(error) => {
                check.coverage = match error {
                    Error::SqlServerError { .. } => Coverage::Diagnosed,
                    _ => Coverage::Inconclusive,
                };
                let message =
                    format!("The session was opened but a minimal query failed: {error}.");
                self.errors.extend(error_records(&error, id));
                (message, true)
            }
            None => {
                check.coverage = Coverage::Inconclusive;
                let message = format!(
                    "The session was opened but a minimal query had no answer within {} s.",
                    self.request.timeout().as_secs()
                );
                let mut record = ErrorRecord::new("client", id, message.clone());
                record.code = Some("timedOut".to_string());
                self.errors.push(record);
                (message, false)
            }
        };
        check.domain = Some(Domain::specific("target.sqlServer"));
        self.checks.push(check);
        if quotes {
            self.quoting_finding(Certainty::Confirmed, id, finding);
        } else {
            self.finding(Certainty::Confirmed, id, finding);
        }
    }
    fn finish(mut self, start_unix_ms: u64, duration_ms: u64) -> Report {
        // The report describes the request; the password was only for logging
        // in, so it is not kept.
        if let Authentication::SqlPassword { password, .. } = &mut self.request.authentication {
            *password = String::new();
        }
        // Checks in their order, whatever order the protocol order ran them in.
        self.checks.sort_by_key(|c| {
            CheckId::ORDER
                .iter()
                .position(|id| *id == c.id)
                .unwrap_or(usize::MAX)
        });
        driver_text::apply(
            &mut self.errors,
            &self.checks,
            self.transport
                .or_else(|| self.target.as_ref().map(|t| t.protocol)),
            &self.request.server,
            driver_text::Encryption {
                encrypt: self.request.encrypt,
                trust_server_certificate: self.request.trust_server_certificate,
            },
        );
        let domain = self
            .checks
            .iter()
            .find(|c| c.coverage.failed())
            .and_then(|c| {
                c.domain.clone().map(|mut domain| {
                    domain.diagnosed = c.coverage == Coverage::Diagnosed;
                    domain
                })
            });
        let outcome = match self.status {
            ExecutionStatus::InvalidInvocation => DiagnosticOutcome::NotEvaluated,
            _ if self.checks.is_empty() => DiagnosticOutcome::NotEvaluated,
            _ => aggregate(&self.checks),
        };
        // The client's evidence ran out where only the driver can see more.
        let tracing_guide = self
            .checks
            .iter()
            .any(|c| {
                matches!(
                    c.id,
                    CheckId::ConnectionAttempt | CheckId::SessionValidation
                ) && matches!(c.coverage, Coverage::Inconclusive | Coverage::Classified)
            })
            .then_some(TRACING_GUIDE);
        Report {
            request: self.request,
            start_unix_ms,
            duration_ms,
            target: self.target,
            checks: self.checks,
            findings: self.findings,
            errors: self.errors,
            authentication: self.authentication,
            server: self.server,
            domain,
            tracing_guide,
            execution_status: self.status,
            outcome,
            limitations: self.limitations,
        }
    }
}

/// The diagnostic outcome from the checks, in the specification's precedence.
fn aggregate(checks: &[Check]) -> DiagnosticOutcome {
    let any = |wanted: &[Coverage]| checks.iter().any(|c| wanted.contains(&c.coverage));
    let operation_failed = checks.iter().any(|c| {
        c.coverage == Coverage::Inconclusive
            && matches!(
                c.id,
                CheckId::ConnectionAttempt | CheckId::SessionValidation
            )
    });
    if operation_failed || any(&[Coverage::Diagnosed, Coverage::Classified]) {
        DiagnosticOutcome::IssueDetected
    } else if any(&[Coverage::Inconclusive]) {
        DiagnosticOutcome::Inconclusive
    } else if any(&[Coverage::Skipped]) {
        DiagnosticOutcome::NotEvaluated
    } else {
        DiagnosticOutcome::Passed
    }
}

/// The resolver's result for a failed lookup, and whether it was read from the
/// message (no stable code).
#[cfg(test)]
fn dns_result(error: &std::io::Error) -> (&'static str, bool) {
    dns_result_from(error.raw_os_error(), &error.to_string())
}

/// [`dns_result`] from the operating-system code and the message, as the
/// client's `dns` stage records them.
fn dns_result_from(os_error: Option<i32>, message: &str) -> (&'static str, bool) {
    match os_error {
        Some(11001) => return ("notFound", false), // WSAHOST_NOT_FOUND
        Some(11002) => return ("temporaryFailure", false), // WSATRY_AGAIN
        Some(11004) => return ("noAddresses", false), // WSANO_DATA
        Some(10060) => return ("timedOut", false),
        _ => {}
    }
    let text = message.to_ascii_lowercase();
    if text.contains("not known")
        || text.contains("no such host")
        || text.contains("nodename nor servname")
        || text.contains("no address associated")
    {
        ("notFound", true)
    } else if text.contains("temporary failure") || text.contains("try again") {
        ("temporaryFailure", true)
    } else {
        ("failed", true)
    }
}

fn version_text(version: Version) -> String {
    format!("{}.{}.{}", version.major, version.minor, version.build)
}

/// The negotiated encryption as `mssql-tds` records it, in sqlcmd's words.
fn encryption_name(negotiated: &str) -> &str {
    match negotiated {
        "Mandatory" => "mandatory",
        "Strict" => "strict",
        "LoginOnly" => "loginOnly",
        "NoEncryption" => "off",
        other => other,
    }
}

/// Whether the user has a Kerberos ticket cache, and where sqlcmd looked:
/// `KRB5CCNAME`, or the default `/tmp/krb5cc_<uid>`. The path itself is never
/// reported.
#[cfg(unix)]
fn kerberos_ticket_cache() -> (&'static str, String) {
    let name = std::env::var("KRB5CCNAME").ok();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    ticket_cache_state(name.as_deref(), uid, |path| {
        std::path::Path::new(path).exists()
    })
}

#[cfg(not(unix))]
fn kerberos_ticket_cache() -> (&'static str, String) {
    ("notApplicable", String::new())
}

/// The ticket cache's state from `KRB5CCNAME` (or its absence) and the user's
/// id: `found` or `notFound` for a file cache, `unverified` for another type
/// (KCM, KEYRING, ...), which sqlcmd cannot inspect. Also where it looked:
/// `KRB5CCNAME`, `default`, or the cache type.
#[cfg_attr(not(unix), allow(dead_code))]
fn ticket_cache_state(
    krb5ccname: Option<&str>,
    uid: u32,
    exists: impl Fn(&str) -> bool,
) -> (&'static str, String) {
    let state = |path: &str| if exists(path) { "found" } else { "notFound" };
    match krb5ccname.filter(|name| !name.is_empty()) {
        Some(name) => match name.split_once(':') {
            Some(("FILE", path)) => (state(path), "KRB5CCNAME".to_string()),
            Some((kind, _)) if !name.starts_with('/') => ("unverified", kind.to_string()),
            _ => (state(name), "KRB5CCNAME".to_string()),
        },
        None => (state(&format!("/tmp/krb5cc_{uid}")), "default".to_string()),
    }
}

/// The finding for a login refused with no ticket cache where sqlcmd looked
/// (`location`, from [`ticket_cache_state`]). Without `KRB5CCNAME` only the
/// default file cache was looked for: krb5.conf can select another, so no
/// missing ticket is claimed beyond it.
fn no_ticket_cache_finding(location: Option<&str>) -> &'static str {
    match location {
        Some("KRB5CCNAME") => {
            "The Kerberos ticket cache KRB5CCNAME names was not found, so the integrated login \
             had no ticket to present."
        }
        _ => {
            "The default file Kerberos ticket cache was not found for this user; unless \
             krb5.conf selects another cache (KCM or KEYRING, which sqlcmd does not inspect), \
             the integrated login had no ticket to present."
        }
    }
}
/// Whether an operating-system error means this computer would not open the
/// connection (permissions, no local address, no sockets left), rather than
/// anything about the target.
fn local_facility_error(code: i32) -> bool {
    if cfg!(windows) {
        // WSAEACCES, WSAEMFILE, WSAEADDRNOTAVAIL, WSAENOBUFS
        matches!(code, 10013 | 10024 | 10049 | 10055)
    } else if cfg!(target_os = "macos") {
        // EPERM, EACCES, ENFILE, EMFILE, EADDRNOTAVAIL, ENOBUFS
        matches!(code, 1 | 13 | 23 | 24 | 49 | 55)
    } else {
        // EPERM, EACCES, ENFILE, EMFILE, EADDRNOTAVAIL, ENOBUFS
        matches!(code, 1 | 13 | 23 | 24 | 99 | 105)
    }
}
/// What the minimal session operation reads.
struct SessionProbe {
    engine_edition: i32,
    /// The server's UTC clock, in milliseconds since the Unix epoch.
    server_unix_ms: Option<i64>,
}

/// The minimal session operation: the engine edition and the server's UTC
/// clock, in one query. The clock is read as days plus milliseconds, which
/// every supported version can compute (`DATEDIFF_BIG` needs 2016).
async fn session_probe(client: &mut TdsClient) -> Result<SessionProbe, Error> {
    let mut result = client
        .execute(
            "SELECT CONVERT(int, SERVERPROPERTY('EngineEdition')), \
             DATEDIFF(day, '19700101', t.now), \
             DATEDIFF(millisecond, CONVERT(datetime2, CONVERT(date, t.now)), t.now) \
             FROM (SELECT SYSUTCDATETIME() AS now) AS t"
                .to_string(),
            (),
        )
        .await?;
    let mut probe = None;
    loop {
        match result {
            StatementResult::Rows => {
                while let Some(row) = client.next_row().await? {
                    if probe.is_none()
                        && let Some(ColumnValues::Int(edition)) = row.first()
                    {
                        let server_unix_ms = match (row.get(1), row.get(2)) {
                            (Some(ColumnValues::Int(days)), Some(ColumnValues::Int(ms))) => {
                                Some(i64::from(*days) * 86_400_000 + i64::from(*ms))
                            }
                            _ => None,
                        };
                        probe = Some(SessionProbe {
                            engine_edition: *edition,
                            server_unix_ms,
                        });
                    }
                }
            }
            StatementResult::NoRows { .. } => {}
            StatementResult::End => break,
        }
        result = client.advance().await?;
    }
    probe.ok_or_else(|| {
        Error::ProtocolError("the session validation query returned no row".to_string())
    })
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
/// The failed phase: the innermost failed one, i.e. the failed one that
/// started last (a TLS handshake that fails inside login fails login too).
/// A phase that was abandoned did not fail; when a time limit ended the
/// attempt (`timed_out`) and none failed, it is the innermost abandoned one,
/// the phase the time ran out in.
fn failed_stage(steps: &[Step], timed_out: bool) -> Option<Stage> {
    let innermost = |pick: fn(&Step) -> bool| {
        steps
            .iter()
            .filter(|step| pick(step))
            .max_by_key(|step| step.started)
            .map(|step| step.stage)
    };
    innermost(Step::failed).or_else(|| {
        if timed_out {
            innermost(Step::abandoned)
        } else {
            None
        }
    })
}

/// Drops the phases up to the last login a routing redirect ended (the
/// gateway's connection), so the phases are those of the connection the
/// client made where it was sent. True when there was such a redirect.
fn followed_redirect(steps: &mut Vec<Step>) -> bool {
    let Some(redirect) = steps
        .iter()
        .filter(|s| s.stage == Stage::Login && s.detail("redirected") == Some("true"))
        .map(|s| s.ended)
        .max()
    else {
        return false;
    };
    steps.retain(|s| s.started > redirect);
    true
}
/// Removes a failed phase that only failed because a phase inside it did (the
/// login around a failed TLS handshake), so the report shows one failure.
fn drop_enclosing_failures(steps: &mut Vec<Step>) {
    let failed: Vec<(Instant, Instant)> = steps
        .iter()
        .filter(|step| step.ok != Some(true))
        .map(|step| (step.started, step.ended))
        .collect();
    steps.retain(|step| {
        step.ok == Some(true)
            || !failed.iter().any(|(start, end)| {
                (*start, *end) != (step.started, step.ended)
                    && *start >= step.started
                    && *end <= step.ended
            })
    });
}

/// Errors from the client, as error-chain records, every server error kept.
fn error_records(error: &Error, check: CheckId) -> Vec<ErrorRecord> {
    match error {
        Error::SqlServerError { diagnostics } if !diagnostics.errors.is_empty() => diagnostics
            .errors
            .iter()
            .map(|e| {
                let mut record = ErrorRecord::new("sqlServer", check, e.message.clone());
                record.number = Some(e.number);
                record.state = Some(e.state);
                record.class = Some(e.class);
                record
            })
            .collect(),
        Error::Io(io) => {
            let mut record = ErrorRecord::new("os", check, io.to_string());
            record.os_error = io.raw_os_error();
            record.code = Some(TcpResult::from_io(io).name().to_string());
            vec![record]
        }
        _ if is_tls_error(error) => vec![ErrorRecord::new("tls", check, error.to_string())],
        // A time limit that ran out, as the session check marks its own.
        Error::TimeoutError(_) => {
            let mut record = ErrorRecord::new("client", check, error.to_string());
            record.code = Some("timedOut".to_string());
            vec![record]
        }
        _ => vec![ErrorRecord::new("client", check, error.to_string())],
    }
}

/// Login errors whose cause is the login itself rather than the network.
const LOGIN_ERRORS: [u32; 6] = [18456, 18452, 18470, 18486, 18487, 18488];
/// The login succeeded but the database could not be opened.
const DATABASE_ERRORS: [u32; 2] = [4060, 4064];

/// A connection-attempt failure, classified.
struct Classified {
    coverage: Coverage,
    domain: Domain,
    auth: AuthOutcome,
    tls_category: Option<&'static str>,
    from_message_text: bool,
    finding: String,
    errors: Vec<ErrorRecord>,
}

fn classify_connection_error(
    error: &Error,
    phase: Option<Stage>,
    authentication: &Authentication,
) -> Classified {
    let errors = error_records(error, CheckId::ConnectionAttempt);
    let auth_domain = match authentication {
        Authentication::SqlPassword { .. } => "security.authentication.sql",
        // Integrated is SSPI on Windows, Kerberos (GSSAPI) elsewhere: the
        // handoff goes to the area that owns it.
        Authentication::Integrated if cfg!(windows) => "security.authentication.windows",
        Authentication::Integrated => "security.authentication.kerberos",
    };
    let reached_login = phase == Some(Stage::Login);

    if let Error::SqlServerError { diagnostics } = error {
        let first = diagnostics.errors.first();
        let number = first.map_or(0, |e| e.number);
        let text = first.map_or_else(|| error.to_string(), |e| e.message.clone());
        let (domain, auth) = if LOGIN_ERRORS.contains(&number) {
            (Domain::specific(auth_domain), AuthOutcome::Failed)
        } else if DATABASE_ERRORS.contains(&number) {
            (Domain::specific("target.sqlServer"), AuthOutcome::Succeeded)
        } else {
            (
                Domain::undetermined(&[auth_domain, "target.sqlServer"]),
                if reached_login {
                    AuthOutcome::Failed
                } else {
                    AuthOutcome::NotEvaluated
                },
            )
        };
        let identifiers = first.map_or(String::new(), |e| {
            format!(" (state {}, class {})", e.state, e.class)
        });
        return Classified {
            coverage: Coverage::Diagnosed,
            domain,
            auth,
            tls_category: None,
            from_message_text: false,
            finding: format!("The server ended the login with error {number}{identifiers}: {text}"),
            errors,
        };
    }

    if let Error::Security(security) = error {
        return Classified {
            coverage: Coverage::Diagnosed,
            domain: Domain::specific(auth_domain),
            auth: AuthOutcome::Failed,
            tls_category: None,
            from_message_text: false,
            finding: format!("Integrated authentication failed on the client: {security}"),
            errors,
        };
    }

    if phase == Some(Stage::Tls) || is_tls_error(error) {
        let category = tls_category(error);
        // The client's own certificate errors are definite evidence; a
        // category read from the error text is weaker.
        let typed = matches!(
            error,
            Error::CertificateExpired | Error::CertificateMismatch | Error::NoServerCertificate
        );
        let (coverage, domain) = match category {
            Some("reset") | None => (
                Coverage::Inconclusive,
                Domain::undetermined(&["security.tlsCertificate", "connectivity.network"]),
            ),
            Some(_) if typed => (
                Coverage::Diagnosed,
                Domain::specific("security.tlsCertificate"),
            ),
            Some(_) => (
                Coverage::Classified,
                Domain::specific("security.tlsCertificate"),
            ),
        };
        let from_message_text = category.is_some() && !typed;
        let mut errors = errors;
        // The records say so too, so the JSON report marks them.
        for record in &mut errors {
            record.from_message_text |= from_message_text;
        }
        return Classified {
            coverage,
            domain,
            auth: AuthOutcome::NotEvaluated,
            tls_category: category,
            from_message_text,
            finding: format!(
                "The TLS handshake failed{}: {error}",
                category.map_or(String::new(), |c| format!(" ({c})"))
            ),
            errors,
        };
    }

    let domain = match error {
        Error::TimeoutError(_) => {
            Domain::undetermined(&["connectivity.network", "target.sqlServer"])
        }
        Error::Io(_) => Domain::undetermined(&["connectivity.network"]),
        _ => Domain::undetermined(&["client.driver", "target.sqlServer"]),
    };
    Classified {
        coverage: Coverage::Inconclusive,
        domain,
        auth: if reached_login {
            AuthOutcome::Failed
        } else {
            AuthOutcome::NotEvaluated
        },
        tls_category: None,
        from_message_text: false,
        finding: format!(
            "The connection attempt failed{}: {error}",
            phase.map_or(String::new(), |p| format!(" during {}", p.title()))
        ),
        errors,
    }
}

/// Runs `work`, which may block (on DNS, say), on its own thread and waits
/// for it at most `budget`, without blocking the runtime, so a cancel is
/// still seen. Work that never finishes leaves its thread behind rather than
/// the diagnosis.
async fn within<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    tokio::time::timeout(budget, receiver).await.ok()?.ok()
}

/// Runs the connection attempt for at most `remaining`, what the steps before
/// it left of the check's time limit (`limit_s`). The client's own limits are
/// whole seconds rounded up, so they alone cannot keep the check within it.
async fn bounded<T>(
    remaining: Duration,
    limit_s: u64,
    attempt: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::time::timeout(remaining, attempt)
        .await
        .unwrap_or_else(|_| {
            Err(Error::TimeoutError(
                mssql_tds::error::TimeoutErrorType::String(format!(
                    "the connection attempt did not finish within the {limit_s} s time limit"
                )),
            ))
        })
}

/// The pipe the client opens first for `target` before it has resolved
/// anything: shared memory to this computer without a prefix (on Windows),
/// or the target's own pipe transport.
fn expected_pipe(target: &Target, windows: bool) -> Option<Protocol> {
    match target.protocol {
        Protocol::Default if windows && target.is_local() && target.port.is_none() => {
            Some(Protocol::SharedMemory)
        }
        Protocol::NamedPipe | Protocol::SharedMemory | Protocol::LocalDb => Some(target.protocol),
        _ => None,
    }
}

/// A SQL Server Browser request this computer would not let out, from the
/// lookup's result code and operating-system error: no UDP socket opened,
/// or every send was refused for a local reason (permission, no resources,
/// no local address).
fn ssrp_local_failure(code: &str, os_error: Option<i32>) -> bool {
    match code {
        "socketFailed" => true,
        "sendFailed" => os_error.is_some_and(local_facility_error),
        _ => false,
    }
}

/// What the SQL Server Browser lookup ran into, from its result code and
/// the underlying error's text (`detail`), as evidence only: the client's
/// own message also says what to do about it, and names the server.
fn ssrp_problem(code: &str, detail: Option<&str>) -> String {
    let detail = |what: &str| match detail {
        Some(detail) => format!("{what}: {detail}"),
        None => what.to_string(),
    };
    match code {
        "nameResolutionFailed" => detail("the server name did not resolve"),
        "noAddresses" => "the server name resolved to no address".into(),
        "socketFailed" => "no UDP socket could be opened".into(),
        "sendFailed" => "no UDP request could be sent".into(),
        "receiveFailed" => detail("receiving the answer failed"),
        // No address: it may be one the report does not list, and so not redact.
        "noAnswer" => "no answer".into(),
        "timedOut" => "no answer in the time limit".into(),
        "invalidResponse" => detail("the answer could not be parsed"),
        "noTcpPort" => "it answered but listed no endpoint the client can use".into(),
        _ => detail("the lookup failed"),
    }
}
fn is_tls_error(error: &Error) -> bool {
    matches!(
        error,
        Error::TlsError(_)
            | Error::TlsHandshakeError { .. }
            | Error::CertificateNotFound { .. }
            | Error::InvalidCertificateFormat { .. }
            | Error::CertificateExpired
            | Error::CertificateMismatch
            | Error::NoServerCertificate
    ) || error
        .to_string()
        .to_ascii_lowercase()
        .contains("tls handshake")
}

/// The kind of TLS failure: `expired`, `trustChain`, `hostnameMismatch`,
/// `protocol`, `cipher`, or `reset` for a handshake the peer ended. Typed
/// errors first, then the error text (Schannel and OpenSSL wording).
fn tls_category(error: &Error) -> Option<&'static str> {
    match error {
        Error::CertificateExpired => return Some("expired"),
        Error::CertificateMismatch => return Some("hostnameMismatch"),
        Error::NoServerCertificate => return Some("trustChain"),
        _ => {}
    }
    let text = error.to_string().to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if has(&[
        "sec_e_cert_expired",
        "certificate has expired",
        "cert_e_expired",
    ]) {
        Some("expired")
    } else if has(&[
        "sec_e_wrong_principal",
        "cert_e_cn_no_match",
        "hostname mismatch",
        "host name mismatch",
    ]) {
        Some("hostnameMismatch")
    } else if has(&[
        "sec_e_untrusted_root",
        "cert_e_untrustedroot",
        "unable to get local issuer",
        "self-signed certificate",
        "self signed certificate",
        "certificate verify failed",
        "not trusted",
    ]) {
        Some("trustChain")
    } else if has(&[
        "sec_e_unsupported_function",
        "unsupported protocol",
        "wrong version number",
        "no protocols available",
    ]) {
        Some("protocol")
    } else if has(&["sec_e_algorithm_mismatch", "no shared cipher"]) {
        Some("cipher")
    } else if has(&[
        "peer closed",
        "connection reset",
        "forcibly closed",
        "unexpected eof",
    ]) {
        Some("reset")
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use mssql_tds::error::{SqlErrorInfo, SqlServerDiagnostics};

    pub(crate) fn request(server: &str, depth: Depth) -> Request {
        Request {
            server: server.to_string(),
            database: None,
            authentication: Authentication::SqlPassword {
                user: "sa".to_string(),
                password: "secret-password".to_string(),
            },
            encrypt: Encrypt::Mandatory,
            trust_server_certificate: true,
            host_name_in_certificate: None,
            login_timeout_seconds: 3,
            depth,
            depth_selected: depth != Depth::SessionValidation,
            local_detail: false,
            invalid: None,
            sources: SettingSources::default(),
        }
    }

    fn step(stage: Stage, ok: bool, start_ms: u64, end_ms: u64, base: Instant) -> Step {
        Step {
            stage,
            ok: Some(ok),
            duration_ms: end_ms - start_ms,
            details: Vec::new(),
            started: base + Duration::from_millis(start_ms),
            ended: base + Duration::from_millis(end_ms),
        }
    }

    fn server_error(number: u32, message: &str) -> Error {
        let mut diagnostics = SqlServerDiagnostics::default();
        diagnostics.errors.push(SqlErrorInfo {
            message: message.to_string(),
            state: 1,
            class: 14,
            number,
            server_name: None,
            proc_name: None,
            line_number: None,
        });
        Error::SqlServerError { diagnostics }
    }

    /// The target type follows `SERVERPROPERTY('EngineEdition')` as documented;
    /// Fabric is not reported as SQL Server, and an unknown edition is not guessed.
    #[test]
    fn the_engine_edition_names_the_target() {
        let target = |edition: i32| {
            ServerInfo {
                name: None,
                version: None,
                database: String::new(),
                encrypted: true,
                engine_edition: Some(edition),
            }
            .target_type()
        };
        for (edition, expected) in [
            (1, "sqlServer"),
            (2, "sqlServer"),
            (3, "sqlServer"),
            (4, "sqlServer"),
            (5, "azureSqlDatabase"),
            (6, "azureSynapseAnalytics"),
            (8, "azureSqlManagedInstance"),
            (9, "azureSqlEdge"),
            (11, "azureSynapseServerlessOrFabric"),
            (12, "fabricSqlDatabase"),
            (7, "unknown"),
            (13, "unknown"),
        ] {
            assert_eq!(target(edition), Some(expected), "edition {edition}");
        }
    }

    /// A login the gateway redirected is not a failure: the phases reported
    /// are those of the connection the client made where it was sent.
    #[test]
    fn a_redirected_login_reports_the_connection_after_it() {
        let base = Instant::now();
        let mut redirected_login = step(Stage::Login, false, 2, 10, base);
        redirected_login
            .details
            .push(("redirected", "true".to_string()));
        let mut steps = vec![
            step(Stage::Prelogin, true, 0, 1, base),
            step(Stage::Tls, true, 1, 2, base),
            redirected_login,
            step(Stage::Prelogin, true, 11, 12, base),
            step(Stage::Tls, true, 12, 14, base),
            step(Stage::Login, true, 14, 30, base),
        ];
        assert!(followed_redirect(&mut steps));
        assert_eq!(steps.len(), 3);
        assert!(steps.iter().all(|s| s.ok == Some(true)), "{steps:#?}");
        assert_eq!(failed_stage(&steps, false), None);

        let mut direct = vec![
            step(Stage::Prelogin, true, 0, 1, base),
            step(Stage::Login, false, 1, 5, base),
        ];
        assert!(!followed_redirect(&mut direct));
        assert_eq!(failed_stage(&direct, false), Some(Stage::Login));
    }

    fn sql_login() -> Authentication {
        Authentication::SqlPassword {
            user: "sa".to_string(),
            password: "x".to_string(),
        }
    }

    #[test]
    fn a_rejected_login_is_diagnosed_as_sql_authentication() {
        let c = classify_connection_error(
            &server_error(18456, "Login failed for user 'sa'."),
            Some(Stage::Login),
            &sql_login(),
        );
        assert_eq!(c.coverage, Coverage::Diagnosed);
        assert_eq!(c.domain, Domain::specific("security.authentication.sql"));
        assert_eq!(c.auth, AuthOutcome::Failed);
        assert_eq!(c.errors[0].number, Some(18456));
        assert_eq!(c.errors[0].class, Some(14));
        assert!(
            c.finding.contains("error 18456 (state 1, class 14)"),
            "{}",
            c.finding
        );
        // Integrated is SSPI on Windows and Kerberos (GSSAPI) elsewhere.
        let integrated = classify_connection_error(
            &server_error(18456, "x"),
            Some(Stage::Login),
            &Authentication::Integrated,
        );
        let expected = if cfg!(windows) {
            "security.authentication.windows"
        } else {
            "security.authentication.kerberos"
        };
        assert_eq!(integrated.domain, Domain::specific(expected));
    }

    #[test]
    fn an_unopenable_database_is_a_target_issue_after_a_good_login() {
        let c = classify_connection_error(
            &server_error(4060, "Cannot open database"),
            Some(Stage::Login),
            &sql_login(),
        );
        assert_eq!(c.domain, Domain::specific("target.sqlServer"));
        assert_eq!(c.auth, AuthOutcome::Succeeded);
    }

    #[test]
    fn tls_failures_are_classified_from_types_then_text() {
        // The client's typed certificate errors are diagnosed; a category
        // read from the text is only classified.
        for (error, category) in [
            (Error::CertificateExpired, "expired"),
            (Error::CertificateMismatch, "hostnameMismatch"),
            (Error::NoServerCertificate, "trustChain"),
        ] {
            let c = classify_connection_error(&error, Some(Stage::Tls), &sql_login());
            assert_eq!(
                (c.coverage, c.tls_category),
                (Coverage::Diagnosed, Some(category))
            );
            assert_eq!(c.domain, Domain::specific("security.tlsCertificate"));
        }
        let typed =
            classify_connection_error(&Error::CertificateExpired, Some(Stage::Tls), &sql_login());
        assert_eq!(typed.coverage, Coverage::Diagnosed);
        assert_eq!(typed.tls_category, Some("expired"));
        assert!(!typed.from_message_text);
        assert!(typed.errors.iter().all(|e| !e.from_message_text));

        let untrusted = classify_connection_error(
            &Error::ImplementationError(
                "Schannel TLS handshake failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)"
                    .to_string(),
            ),
            Some(Stage::Tls),
            &sql_login(),
        );
        assert_eq!(untrusted.tls_category, Some("trustChain"));
        assert_eq!(untrusted.coverage, Coverage::Classified);
        assert_eq!(
            untrusted.domain,
            Domain::specific("security.tlsCertificate")
        );
        assert!(untrusted.from_message_text);
        assert!(untrusted.errors.iter().all(|e| e.from_message_text));
        assert_eq!(untrusted.auth, AuthOutcome::NotEvaluated);

        let reset = classify_connection_error(
            &Error::ImplementationError(
                "Schannel TLS handshake failed: peer closed connection during TLS handshake"
                    .to_string(),
            ),
            Some(Stage::Tls),
            &sql_login(),
        );
        assert_eq!(
            reset.coverage,
            Coverage::Inconclusive,
            "a reset is not specific"
        );
        assert_eq!(reset.domain.primary, "undetermined");
    }

    #[test]
    fn a_generic_timeout_selects_no_specific_domain() {
        let c = classify_connection_error(
            &Error::TimeoutError(mssql_tds::error::TimeoutErrorType::String("x".to_string())),
            Some(Stage::Prelogin),
            &sql_login(),
        );
        assert_eq!(c.coverage, Coverage::Inconclusive);
        assert_eq!(c.domain.primary, "undetermined");
    }

    #[test]
    fn tcp_results_follow_the_operating_system_codes() {
        use std::io::{Error as IoError, ErrorKind};
        assert_eq!(
            TcpResult::from_io(&IoError::from(ErrorKind::ConnectionRefused)),
            TcpResult::Refused
        );
        let (refused, timed_out, host, network, other_platform) = if cfg!(windows) {
            (10061, 10060, 10065, 10051, 111)
        } else if cfg!(target_os = "macos") {
            (61, 60, 65, 51, 111)
        } else {
            // On Linux 61 is ENODATA, not a refusal.
            (111, 110, 113, 101, 61)
        };
        for (code, expected) in [
            (refused, TcpResult::Refused),
            (timed_out, TcpResult::TimedOut),
            (host, TcpResult::HostUnreachable),
            (network, TcpResult::NetworkUnreachable),
            (other_platform, TcpResult::Failed),
        ] {
            assert_eq!(
                TcpResult::from_io(&IoError::from_raw_os_error(code)),
                expected,
                "{code}"
            );
            assert_eq!(tcp_result_from_code(code), expected, "{code}");
        }
        assert_eq!(TcpResult::from_io(&IoError::other("x")), TcpResult::Failed);
    }
    #[test]
    fn dns_results_prefer_codes_and_fall_back_to_text() {
        assert_eq!(
            dns_result(&std::io::Error::from_raw_os_error(11001)),
            ("notFound", false)
        );
        assert_eq!(
            dns_result(&std::io::Error::from_raw_os_error(11002)),
            ("temporaryFailure", false)
        );
        assert_eq!(
            dns_result(&std::io::Error::other(
                "failed to lookup address information: Name or service not known"
            )),
            ("notFound", true)
        );
        assert_eq!(
            dns_result(&std::io::Error::other("strange")),
            ("failed", true)
        );
    }

    #[test]
    fn a_failed_handshake_inside_the_login_is_reported_once() {
        let base = Instant::now();
        let mut steps = vec![
            step(Stage::Prelogin, true, 0, 1, base),
            step(Stage::Tls, false, 2, 9, base),
            step(Stage::Login, false, 1, 9, base),
        ];
        assert_eq!(failed_stage(&steps, false), Some(Stage::Tls));
        drop_enclosing_failures(&mut steps);
        let stages: Vec<Stage> = steps.iter().map(|s| s.stage).collect();
        assert_eq!(stages, [Stage::Prelogin, Stage::Tls]);
    }

    /// The report, not just the helper, drops the login that failed only
    /// because the TLS handshake inside it did: the attempt shows pre-login
    /// and TLS, with TLS the one failed phase, in JSON and text.
    #[test]
    fn the_report_shows_a_failed_handshake_inside_the_login_once() {
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                1,
                2,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            )
            .add(Stage::Prelogin, Some(true), 2, 3, &[])
            .add(Stage::Login, Some(false), 3, 10, &[])
            .add(Stage::Tls, Some(false), 4, 9, &[]);
        let report = observed(
            request("tcp:db01,1433", Depth::ConnectionAttempt),
            stages,
            Ended::Failed(Error::ProtocolError("TLS handshake failed".to_string())),
        );
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        let phases: Vec<Stage> = attempt.subphases.iter().map(|s| s.stage).collect();
        assert_eq!(phases, [Stage::Prelogin, Stage::Tls], "{attempt:#?}");

        let value: serde_json::Value =
            serde_json::from_str(&report::json(&report, "v").unwrap()).unwrap();
        let check = value["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == "connectionAttempt")
            .unwrap();
        let listed: Vec<(&str, &str)> = check["phases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["phase"].as_str().unwrap(), p["status"].as_str().unwrap()))
            .collect();
        assert_eq!(
            listed,
            [("prelogin", "passed"), ("tls", "failed")],
            "{check:#}"
        );

        let text = report::text(&report);
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with("Connection attempt"))
            .unwrap();
        assert_eq!(line.matches("FAILED").count(), 1, "{text}");
        assert!(!line.contains(", login"), "{text}");
    }

    #[test]
    fn the_outcome_follows_the_specification_precedence() {
        use Coverage::*;
        let checks = |list: &[(CheckId, Coverage)]| -> Vec<Check> {
            list.iter().map(|(id, c)| Check::new(*id, *c)).collect()
        };
        assert_eq!(
            aggregate(&checks(&[
                (CheckId::ConnectionInput, Passed),
                (CheckId::InstanceResolution, NotApplicable)
            ])),
            DiagnosticOutcome::Passed
        );
        assert_eq!(
            aggregate(&checks(&[
                (CheckId::TcpConnect, Diagnosed),
                (CheckId::ConnectionAttempt, Skipped)
            ])),
            DiagnosticOutcome::IssueDetected
        );
        assert_eq!(
            aggregate(&checks(&[
                (CheckId::InstanceResolution, Inconclusive),
                (CheckId::TcpConnect, Skipped)
            ])),
            DiagnosticOutcome::Inconclusive
        );
        assert_eq!(
            aggregate(&checks(&[(CheckId::ConnectionAttempt, Inconclusive)])),
            DiagnosticOutcome::IssueDetected,
            "a known failed operation stays issueDetected"
        );
        assert_eq!(
            aggregate(&checks(&[(CheckId::TcpConnect, Skipped)])),
            DiagnosticOutcome::NotEvaluated
        );
    }

    #[test]
    fn exit_codes_follow_the_category_precedence() {
        let mut report = run(request("db,abc", Depth::SessionValidation));
        assert_eq!(report.exit_category(), ExitCategory::IssueDetected);
        assert_eq!(report.exit_code(), 1);
        report.execution_status = ExecutionStatus::Partial;
        assert_eq!(report.exit_code(), 3, "partial outranks issueDetected");
        report.execution_status = ExecutionStatus::InvalidInvocation;
        assert_eq!(report.exit_code(), 6);
    }

    #[test]
    fn an_invalid_request_runs_nothing() {
        let mut invalid = request("db01", Depth::SessionValidation);
        invalid.invalid = Some("-Q cannot be used".to_string());
        let report = run(invalid);
        assert_eq!(report.execution_status, ExecutionStatus::InvalidInvocation);
        assert_eq!(report.outcome, DiagnosticOutcome::NotEvaluated);
        assert!(report.checks.is_empty());
        assert_eq!(report.exit_code(), 6);
        assert_eq!(report.errors[0].code.as_deref(), Some("invalidInvocation"));
    }

    #[test]
    fn connection_input_depth_makes_no_external_call() {
        let report = run(request(
            "tcp:no-such-host.invalid,1433",
            Depth::ConnectionInput,
        ));
        let ids: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
        assert_eq!(ids, [CheckId::ConnectionInput]);
        assert_eq!(report.outcome, DiagnosticOutcome::Passed);
        assert_eq!(
            report.deepest_phase_evaluated(),
            Some(Depth::ConnectionInput)
        );
    }

    #[test]
    fn connection_input_labels_the_database() {
        let mut with_database = request("tcp:db01,1433", Depth::ConnectionInput);
        with_database.database = Some("sales".to_string());
        let mut report = run(with_database);
        // The measured duration, set to 0 so the rendered time is always `<1 ms`.
        report.checks[0].duration_ms = Some(0);
        let text = report::text(&report);
        assert!(
            text.contains("  Connection input      PASSED           <1 ms  tcp, host db01, port 1433, database database-1\n"),
            "{text}"
        );
        assert!(!text.contains("sales"), "{text}");

        let report = run(request("tcp:db01,1433", Depth::ConnectionInput));
        assert!(report.checks[0].field("database").is_none());
    }

    #[test]
    fn connection_input_reports_setting_sources() {
        let mut with_sources = request("tcp:db01,1433", Depth::ConnectionInput);
        with_sources.database = Some("sales".to_string());
        with_sources.sources = SettingSources {
            server: Some(SettingOrigin::command_line("-S")),
            database: Some(SettingOrigin::default_source()),
            user: Some(SettingOrigin::environment("SQLCMDUSER")),
            authentication: Some(SettingOrigin::environment("SQLCMDUSER")),
            encrypt: Some(SettingOrigin::default_source()),
            trust_server_certificate: Some(SettingOrigin::command_line("-C")),
            host_name_in_certificate: None,
            login_timeout: Some(SettingOrigin::command_line("-l")),
        };

        let report = run(with_sources);
        let text = report::text(&report);
        let sources = "server from -S, login from SQLCMDUSER, authentication from SQLCMDUSER, database default, encrypt default, certificate trust from -C, time limit from -l\n";
        let row = text
            .lines()
            .position(|line| line.starts_with("  Connection input"))
            .unwrap();
        assert_eq!(
            text.lines().nth(row + 1).map(str::trim_start),
            Some(sources.trim_end()),
            "{text}"
        );
        assert!(!text.contains("Settings:"), "{text}");
        let json = report::json(&report, "18.7.0001.1").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["target"].get("sources").is_none(), "{json}");
        let input_sources = &value["checks"][0]["sources"];
        assert_eq!(value["checks"][0]["check"], "connectionInput");
        assert_eq!(
            input_sources["server"],
            serde_json::json!({ "source": "commandLine", "name": "-S" })
        );
        assert_eq!(
            input_sources["database"],
            serde_json::json!({ "source": "default" })
        );
        assert_eq!(
            input_sources["user"],
            serde_json::json!({ "source": "environment", "name": "SQLCMDUSER" })
        );
        assert!(
            value["checks"]
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .all(|check| check.get("sources").is_none()),
            "{json}"
        );
    }

    #[test]
    fn connection_input_reports_local_validation_findings() {
        let mut request = request("tcp:db01\\SQL2022,1433", Depth::ConnectionInput);
        request.host_name_in_certificate = Some("wrong.example.test".to_string());
        request.trust_server_certificate = true;

        let report = run(request);
        assert_eq!(report.checks[0].coverage, Coverage::Passed);
        let findings: Vec<_> = report.findings.iter().map(|f| f.text.as_str()).collect();
        assert!(
            findings
                .iter()
                .any(|text| text.contains("uses the port and does not look up the instance")),
            "{findings:#?}"
        );
        assert!(
            findings
                .iter()
                .any(|text| text.contains("not validated when -C trusts the server certificate")),
            "{findings:#?}"
        );
        assert!(
            report
                .findings
                .iter()
                .all(|finding| finding.certainty == Certainty::Informational),
            "{:#?}",
            report.findings
        );
    }

    #[test]
    fn strict_encryption_ignores_trust_and_does_not_report_it_as_trusted() {
        let mut request = request("db01", Depth::ConnectionInput);
        request.encrypt = Encrypt::Strict;
        request.trust_server_certificate = true;
        request.host_name_in_certificate = Some("wrong.example.test".to_string());

        let report = run(request);
        let findings: Vec<_> = report.findings.iter().map(|f| f.text.as_str()).collect();
        assert!(
            findings
                .iter()
                .any(|text| text.contains("-C is ignored with strict encryption")),
            "{findings:#?}"
        );
        assert!(
            findings.iter().all(|text| !text.contains("not validated")),
            "the certificate name is validated in strict mode: {findings:#?}"
        );
        let text = report::text(&report);
        assert!(text.contains("-C ignored"), "{text}");
        assert!(!text.contains("server certificate trusted"), "{text}");
    }

    #[test]
    fn a_parse_failure_skips_every_later_check_in_scope() {
        let report = run(request("db01,99999", Depth::NetworkReachability));
        let summary: Vec<(CheckId, Coverage)> =
            report.checks.iter().map(|c| (c.id, c.coverage)).collect();
        assert_eq!(
            summary,
            [
                (CheckId::ConnectionInput, Coverage::Diagnosed),
                (CheckId::NameResolution, Coverage::Skipped),
                (CheckId::InstanceResolution, Coverage::Skipped),
                (CheckId::TcpConnect, Coverage::Skipped),
            ]
        );
        assert_eq!(
            report.checks[3].skip,
            Some(Skip::BlockedBy(CheckId::ConnectionInput))
        );
        assert_eq!(
            report.domain,
            Some(Domain {
                diagnosed: true,
                ..Domain::specific("client.inputConfiguration")
            })
        );
        assert_eq!(report.errors[0].code.as_deref(), Some("invalidPort"));
    }

    /// A space before an instance after the port (`tcp:host, 50001 \SQL2022`)
    /// is in the client's port, which it then refuses: Connection input
    /// fails, at any depth, and nothing after it runs. Without that space
    /// the port passes.
    #[test]
    fn a_space_before_the_instance_after_the_port_fails_connection_input() {
        for server in [
            "tcp:127.0.0.1, 50001 \\SQL2022",
            "127.0.0.1,50001 \\SQL2022",
        ] {
            for depth in [Depth::ConnectionInput, Depth::EndpointResolution] {
                let report = run(request(server, depth));
                assert_eq!(report.checks[0].id, CheckId::ConnectionInput);
                assert_eq!(report.checks[0].coverage, Coverage::Diagnosed, "{server}");
                assert_eq!(
                    report.errors[0].code.as_deref(),
                    Some("invalidPort"),
                    "{server}: {:#?}",
                    report.errors
                );
                assert!(
                    report
                        .errors
                        .iter()
                        .all(|e| e.check == CheckId::ConnectionInput),
                    "{server}: {:#?}",
                    report.errors
                );
                assert!(
                    report.checks[1..]
                        .iter()
                        .all(|c| c.skip == Some(Skip::BlockedBy(CheckId::ConnectionInput))),
                    "{server}: {:#?}",
                    report.checks
                );
                assert_eq!(report.exit_code(), 1, "{server}");
            }
        }
        for server in ["tcp:127.0.0.1, 50001\\SQL2022", "tcp:127.0.0.1 , 50001 "] {
            let report = run(request(server, Depth::ConnectionInput));
            assert_eq!(report.checks[0].coverage, Coverage::Passed, "{server}");
            assert_eq!(report.exit_code(), 0, "{server}");
        }
    }

    /// An empty port and port 0 pass Connection input, as the client takes
    /// them: an empty port is 1433, or for a named instance before it the
    /// port SQL Server Browser gives; port 0 is a port, which the connect
    /// then fails on.
    #[test]
    fn an_empty_port_and_port_0_pass_connection_input_as_for_the_client() {
        let field = |report: &Report, key: &str| {
            report.checks[0]
                .fields
                .iter()
                .find(|f| f.key == key)
                .map(|f| f.value.clone())
        };
        for (server, port, instance) in [
            ("tcp:127.0.0.1,", Some("1433"), None),
            ("127.0.0.1, ", Some("1433"), None),
            ("127.0.0.1,\\SQL2022", Some("1433"), None),
            ("tcp:127.0.0.1,0", Some("0"), None),
            ("127.0.0.1\\SQL2022,", None, Some("SQL2022")),
        ] {
            let report = run(request(server, Depth::ConnectionInput));
            assert_eq!(report.checks[0].coverage, Coverage::Passed, "{server}");
            assert!(report.errors.is_empty(), "{server}: {:#?}", report.errors);
            assert_eq!(field(&report, "port").as_deref(), port, "{server}");
            assert_eq!(field(&report, "instance").as_deref(), instance, "{server}");
            assert_eq!(report.exit_code(), 0, "{server}");
        }
        // The reserved name with an empty port is refused, as with none.
        let report = run(request("127.0.0.1\\MSSQLSERVER,", Depth::ConnectionInput));
        assert_eq!(report.checks[0].coverage, Coverage::Diagnosed);
        assert_eq!(report.exit_code(), 1);
    }

    /// Port 0 passes Connection input, as the client takes it, and every
    /// connect to it fails: Windows says the address is not valid (10049),
    /// which is not this computer refusing to connect, and elsewhere the
    /// connect is refused. Either way TCP connect is diagnosed, with the port
    /// as the area, not a partial run.
    #[test]
    fn port_0_fails_tcp_connect_as_the_port_given() {
        let port_0 = |os_error: &str, io_kind: &str, error: &str| {
            Stages::new()
                .add(
                    Stage::Dns,
                    Some(true),
                    0,
                    1,
                    &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1, 10.0.0.2")],
                )
                .add(
                    Stage::Tcp,
                    Some(false),
                    1,
                    2,
                    &[
                        (cs::ADDRESS, "10.0.0.1"),
                        (cs::PORT, "0"),
                        (cs::OS_ERROR, os_error),
                        (cs::IO_KIND, io_kind),
                        (cs::ERROR, error),
                    ],
                )
                .add(
                    Stage::Tcp,
                    Some(false),
                    2,
                    3,
                    &[
                        (cs::ADDRESS, "10.0.0.2"),
                        (cs::PORT, "0"),
                        (cs::OS_ERROR, os_error),
                        (cs::IO_KIND, io_kind),
                        (cs::ERROR, error),
                    ],
                )
        };
        for stages in [
            port_0(
                "10049",
                "AddrNotAvailable",
                "The requested address is not valid in its context. (os error 10049)",
            ),
            port_0(
                "111",
                "ConnectionRefused",
                "Connection refused (os error 111)",
            ),
        ] {
            let report = observed(
                request("tcp:db01,0", Depth::NetworkReachability),
                stages,
                Ended::Failed(Error::Io(std::io::Error::from(
                    std::io::ErrorKind::AddrNotAvailable,
                ))),
            );
            assert_eq!(
                checked(&report, CheckId::ConnectionInput).coverage,
                Coverage::Passed
            );
            let check = checked(&report, CheckId::TcpConnect);
            assert_eq!(check.coverage, Coverage::Diagnosed, "{report:#?}");
            assert_eq!(check.field("port"), Some("0"));
            assert_eq!(check.attempts.len(), 2);
            assert_eq!(report.execution_status, ExecutionStatus::Completed);
            assert!(report.limitations.is_empty(), "{:#?}", report.limitations);
            assert_eq!(
                report.domain.as_ref().map(|d| d.primary),
                Some("client.inputConfiguration")
            );
            assert!(
                report
                    .findings
                    .iter()
                    .any(|f| f.text.contains("The port is 0")),
                "{:#?}",
                report.findings
            );
            assert_eq!(report.outcome, DiagnosticOutcome::IssueDetected);
            assert_eq!(report.exit_code(), 1);
        }

        // Live, on this platform: the connect to port 0 fails at once, and
        // at the default depth the connection attempt is not made.
        for depth in [Depth::NetworkReachability, Depth::SessionValidation] {
            let report = run(request("tcp:127.0.0.1,0", depth));
            assert_eq!(report.checks[0].coverage, Coverage::Passed, "{report:#?}");
            let check = checked(&report, CheckId::TcpConnect);
            assert_eq!(check.coverage, Coverage::Diagnosed, "{report:#?}");
            if depth == Depth::SessionValidation {
                assert_eq!(
                    checked(&report, CheckId::ConnectionAttempt).skip,
                    Some(Skip::BlockedBy(CheckId::TcpConnect))
                );
            }
            assert_eq!(report.exit_code(), 1, "{report:#?}");
        }
    }
    /// Shared memory to another computer fails Connection input, as the
    /// client refuses it, rather than passing a server the client rejects.
    #[test]
    fn shared_memory_to_another_computer_fails_connection_input() {
        let report = run(request("lpc:remoteserver", Depth::ConnectionInput));
        assert_eq!(report.checks[0].coverage, Coverage::Diagnosed);
        let expected = if cfg!(windows) {
            "sharedMemoryNotLocal"
        } else {
            "sharedMemoryNotSupported"
        };
        assert_eq!(report.errors[0].code.as_deref(), Some(expected));
        assert_eq!(report.exit_code(), 1);
    }

    /// [`run`] as on Windows (`windows`) or not.
    fn run_on(request: Request, windows: bool) -> Report {
        run_until_on(request, &|| false, windows)
    }

    /// Off Windows the client refuses LocalDB and named pipes before it
    /// connects, so Connection input fails, at any depth, and nothing after
    /// it runs; on Windows the same servers pass it.
    #[test]
    fn windows_only_servers_fail_connection_input_off_windows() {
        let cases = [
            ("(localdb)\\MSSQLLocalDB", "localDbNotSupported"),
            ("np:db01", "namedPipesNotSupported"),
            ("\\\\db01\\pipe\\sql\\query", "namedPipesNotSupported"),
            ("lpc:.", "sharedMemoryNotSupported"),
        ];
        for (server, code) in cases {
            for depth in [Depth::ConnectionInput, Depth::SessionValidation] {
                let report = run_on(request(server, depth), false);
                assert_eq!(report.checks[0].id, CheckId::ConnectionInput);
                assert_eq!(report.checks[0].coverage, Coverage::Diagnosed, "{server}");
                assert_eq!(report.errors[0].code.as_deref(), Some(code), "{server}");
                assert!(
                    report.checks[1..]
                        .iter()
                        .all(|c| c.skip == Some(Skip::BlockedBy(CheckId::ConnectionInput))),
                    "{server}: {:#?}",
                    report.checks
                );
                assert_eq!(report.exit_code(), 1, "{server}");
            }
            let report = run_on(request(server, Depth::ConnectionInput), true);
            assert_eq!(report.checks[0].coverage, Coverage::Passed, "{server}");
            assert_eq!(report.exit_code(), 0, "{server}");
        }
        // The client's message keeps its one closing period in the finding.
        let report = run_on(
            request("(localdb)\\MSSQLLocalDB", Depth::ConnectionInput),
            false,
        );
        assert!(
            report.findings[0]
                .text
                .ends_with("(e.g., 'tcp:server,port')."),
            "{:#?}",
            report.findings
        );
    }

    /// An empty instance after `\` is the default instance, as for the
    /// client: Connection input passes it, with no instance.
    #[test]
    fn an_empty_instance_passes_connection_input_as_the_default_instance() {
        for server in ["tcp:127.0.0.1\\", "127.0.0.1\\", "tcp:127.0.0.1\\,1433"] {
            let report = run(request(server, Depth::ConnectionInput));
            assert_eq!(report.checks[0].coverage, Coverage::Passed, "{server}");
            assert!(
                report.checks[0].fields.iter().all(|f| f.key != "instance"),
                "{server}: {:#?}",
                report.checks[0]
            );
            assert_eq!(report.exit_code(), 0, "{server}");
        }
        let report = run_on(request("np:127.0.0.1\\", Depth::ConnectionInput), true);
        assert_eq!(report.checks[0].coverage, Coverage::Passed);
        let report = run_on(request("(localdb)\\", Depth::ConnectionInput), true);
        assert_eq!(report.errors[0].code.as_deref(), Some("missingInstance"));
    }

    /// Connection input names no transport for an unprefixed server without a
    /// port on Windows (`auto`: the client tries shared memory, TCP and named
    /// pipes in turn); a port makes it TCP, as for the client, and off
    /// Windows the client has only TCP.
    #[test]
    fn connection_input_reports_the_protocol_the_client_will_use() {
        for (server, windows, expected) in [
            (".", true, "auto"),
            ("db01", true, "auto"),
            ("db01\\SQL2022", true, "auto"),
            ("db01,1433", true, "tcp"),
            ("db01,", true, "tcp"),
            ("db01\\SQL2022,", true, "tcp"),
            ("tcp:db01", true, "tcp"),
            (".", false, "tcp"),
            ("db01", false, "tcp"),
            ("db01,1433", false, "tcp"),
        ] {
            let report = run_on(request(server, Depth::ConnectionInput), windows);
            let protocol = report.checks[0]
                .fields
                .iter()
                .find(|f| f.key == "protocol")
                .map(|f| f.value.clone());
            assert_eq!(
                protocol.as_deref(),
                Some(expected),
                "{server} windows={windows}"
            );
        }
    }

    /// A pipe path after `tcp:`, `admin:` or `lpc:` is a named pipe, as the
    /// client takes it: Connection input passes it on Windows, with the pipe's
    /// host, and refuses it off Windows as it does a bare pipe path.
    #[test]
    fn a_pipe_path_after_any_prefix_passes_connection_input_as_a_named_pipe() {
        let pipe = "\\\\127.0.0.1\\pipe\\sql\\query";
        for prefix in ["", "np:", "tcp:", "admin:", "lpc:"] {
            let server = format!("{prefix}{pipe}");
            let report = run_on(request(&server, Depth::ConnectionInput), true);
            assert_eq!(report.checks[0].coverage, Coverage::Passed, "{server}");
            let field = |key: &str| {
                report.checks[0]
                    .fields
                    .iter()
                    .find(|f| f.key == key)
                    .map(|f| f.value.clone())
            };
            assert_eq!(field("protocol").as_deref(), Some("namedPipe"), "{server}");
            assert_eq!(field("host").as_deref(), Some("127.0.0.1"), "{server}");
            assert!(report.errors.is_empty(), "{server}: {:#?}", report.errors);
            assert_eq!(report.exit_code(), 0, "{server}");
            for depth in [Depth::ConnectionInput, Depth::SessionValidation] {
                let report = run_on(request(&server, depth), false);
                assert_eq!(
                    report.errors[0].code.as_deref(),
                    Some("namedPipesNotSupported"),
                    "{server}"
                );
                assert_eq!(report.exit_code(), 1, "{server}");
            }
        }
    }

    /// An unknown prefix is part of the server name, as for the client:
    /// Connection input passes it on, on either platform.
    #[test]
    fn an_unknown_prefix_passes_connection_input_as_part_of_the_host() {
        for windows in [true, false] {
            let report = run_on(request("udp:db01", Depth::ConnectionInput), windows);
            assert_eq!(report.checks[0].coverage, Coverage::Passed);
            let host = report.checks[0].fields.iter().find(|f| f.key == "host");
            assert_eq!(host.map(|f| f.value.as_str()), Some("udp:db01"));
            assert_eq!(report.exit_code(), 0);
        }
    }

    /// Port 1 on the loopback address refuses at once: name resolution
    /// passes, TCP is diagnosed, and the connection attempt is skipped.
    #[test]
    fn a_refused_port_reads_as_a_normal_sqlcmd_run() {
        let report = run(request("tcp:127.0.0.1,1", Depth::NetworkReachability));
        let codes: Vec<Option<&str>> = report.errors.iter().map(|e| e.code.as_deref()).collect();
        let tcp_text = if cfg!(windows) {
            "TCP Provider: No connection could be made because the target machine actively \
             refused it"
        } else {
            "TCP Provider: Error code 0x274D"
        };
        // Refused at once: no time limit ran out, so no timeout summary.
        let expected = vec![Some("refused"), Some("networkRelatedError")];
        assert_eq!(codes, expected, "{:#?}", report.errors);
        let refused = report.errors.iter().find(|e| e.source == "os").unwrap();
        assert_eq!(refused.text.as_deref(), Some(tcp_text));
        let network = report.errors.last().unwrap();
        assert!(
            network
                .text
                .as_deref()
                .is_some_and(|t| t.contains("connection to tcp:127.0.0.1,1. Server is not found")),
            "{network:#?}"
        );
    }

    #[test]
    fn a_refused_port_is_diagnosed_without_a_server() {
        let report = run(request("tcp:127.0.0.1,1", Depth::SessionValidation));
        let tcp = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::TcpConnect)
            .unwrap();
        assert_eq!(tcp.coverage, Coverage::Diagnosed);
        assert_eq!(tcp.attempts[0].result, TcpResult::Refused);
        assert!(tcp.attempts[0].os_error.is_some());
        let attempt = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::ConnectionAttempt)
            .unwrap();
        assert_eq!(attempt.skip, Some(Skip::BlockedBy(CheckId::TcpConnect)));
        assert_eq!(report.outcome, DiagnosticOutcome::IssueDetected);
        assert_eq!(report.domain.as_ref().unwrap().primary, "undetermined");
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.certainty == Certainty::Suspected)
        );
        assert_eq!(
            report.deepest_phase_evaluated(),
            Some(Depth::NetworkReachability)
        );
        assert_eq!(report.exit_code(), 1);
    }

    /// A live server over a named pipe, when `MSSQL_SQLCMD_DIAGNOSTICS_PIPE`
    /// names one, in any case (`\\.\PIPE\sql\query`), with the `..._USER`
    /// and `..._PASSWORD` of the live test: passes through session validation.
    #[cfg(windows)]
    #[test]
    fn a_live_named_pipe_passes_whatever_its_case() {
        let Ok(pipe) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_PIPE") else {
            return;
        };
        let mut live = request(&pipe, Depth::SessionValidation);
        live.authentication = Authentication::SqlPassword {
            user: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_USER").unwrap_or_default(),
            password: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_PASSWORD").unwrap_or_default(),
        };
        let report = run(live);
        assert_eq!(
            report.outcome,
            DiagnosticOutcome::Passed,
            "{pipe}: {report:#?}"
        );
    }
    /// A live server over IPv6, when `MSSQL_SQLCMD_DIAGNOSTICS_IPV6` names a
    /// bare IPv6 target such as `::1,1433` (with the `..._USER` and
    /// `..._PASSWORD` of the live test): passes through session validation
    /// with and without the `tcp:` prefix.
    /// Without a prefix, on Windows, a name that does not resolve is tried
    /// over named pipes next, as the client does: the report ends with the
    /// errors a normal run prints for it.
    #[cfg(windows)]
    #[test]
    fn without_a_prefix_a_failed_tcp_path_falls_back_to_named_pipes() {
        let report = run(request("nosuchhost.invalid", Depth::SessionValidation));
        let attempt = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::ConnectionAttempt)
            .unwrap();
        assert!(attempt.coverage.failed(), "{attempt:#?}");
        assert_eq!(attempt.field("transport"), Some("namedPipe"));
        assert_eq!(
            attempt.field("pipe"),
            Some(r"\\nosuchhost.invalid\pipe\sql\query")
        );
        let resolution = report
            .errors
            .iter()
            .find(|e| e.check == CheckId::NameResolution)
            .unwrap();
        assert!(
            resolution.superseded && resolution.text.is_none(),
            "{resolution:#?}"
        );
        let printed: Vec<&str> = report
            .errors
            .iter()
            .filter(|e| !e.superseded)
            .filter_map(|e| e.text.as_deref())
            .collect();
        // The pipe was not found: no time limit ran out, so no timeout line.
        assert_eq!(printed.len(), 2, "{:#?}", report.errors);
        assert!(
            printed[0]
                .starts_with("Named Pipes Provider: Could not open a connection to SQL Server ["),
            "{printed:#?}"
        );
        assert!(printed[1].starts_with("A network-related"), "{printed:#?}");
        let order: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
        assert_eq!(order, CheckId::ORDER, "checks stay in their order");
    }

    /// Elsewhere there is no other transport: the TCP path's failure blocks
    /// the attempt.
    #[cfg(not(windows))]
    #[test]
    fn without_a_prefix_only_tcp_is_tried_off_windows() {
        let report = run(request("nosuchhost.invalid", Depth::SessionValidation));
        let attempt = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::ConnectionAttempt)
            .unwrap();
        assert_eq!(attempt.skip, Some(Skip::BlockedBy(CheckId::NameResolution)));
        assert!(report.errors.iter().all(|e| !e.superseded));
    }

    /// Shared memory to an instance that is not there cannot be opened: the
    /// attempt is set aside, noted, and the TCP checks follow.
    #[cfg(windows)]
    #[test]
    fn shared_memory_that_does_not_open_is_set_aside_for_tcp() {
        let report = run(request(
            r"localhost\NOSUCHINSTANCE1",
            Depth::SessionValidation,
        ));
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.certainty == Certainty::Informational
                    && f.text
                        .starts_with("Shared memory to this computer did not open")),
            "{:#?}",
            report.findings
        );
        let resolution = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::NameResolution)
            .unwrap();
        assert_eq!(resolution.coverage, Coverage::Passed, "TCP checks ran");
        let attempts: Vec<&Check> = report
            .checks
            .iter()
            .filter(|c| c.id == CheckId::ConnectionAttempt)
            .collect();
        assert_eq!(attempts.len(), 1, "{:#?}", report.checks);
        // The attempt left is the TCP path's: skipped, as the named instance
        // has no port, not the shared-memory one that was set aside.
        assert_eq!(
            attempts[0].skip,
            Some(Skip::PortRequired),
            "{:#?}",
            attempts[0]
        );
        // Shared memory's error is kept as evidence only, marked superseded.
        assert!(
            report
                .errors
                .iter()
                .all(|e| e.check != CheckId::ConnectionAttempt || e.superseded),
            "{:#?}",
            report.errors
        );
        assert!(
            report
                .checks
                .iter()
                .all(|c| c.field("transport") != Some("sharedMemory")),
            "{:#?}",
            report.checks
        );
    }

    /// An unknown prefix is part of the server name, as for the client: the
    /// whole name is resolved, and its resolution fails.
    #[test]
    fn an_unknown_prefix_fails_name_resolution_for_the_whole_name() {
        let report = run(request("udp:db01", Depth::EndpointResolution));
        assert_eq!(
            checked(&report, CheckId::ConnectionInput).coverage,
            Coverage::Passed
        );
        let name = checked(&report, CheckId::NameResolution);
        assert_eq!(name.field("host"), Some("udp:db01"), "{report:#?}");
        assert_eq!(name.field("result"), Some("notFound"), "{name:#?}");
        assert!(name.coverage.failed(), "{name:#?}");
        assert_eq!(report.exit_code(), 1);
    }

    /// A local named instance without a prefix, at a depth that stops after
    /// resolution: the client would try shared memory before SQL Server
    /// Browser, so the instance was not looked up, which is inconclusive, not
    /// a pass. A default instance there has nothing to look up.
    #[cfg(windows)]
    #[test]
    fn a_local_named_instance_is_not_looked_up_before_shared_memory() {
        let report = run(request(
            ".\\NoSuchDiagnoseInstance",
            Depth::EndpointResolution,
        ));
        let instance = checked(&report, CheckId::InstanceResolution);
        assert_eq!(instance.coverage, Coverage::Inconclusive, "{report:#?}");
        assert_eq!(instance.field("result"), Some("notLookedUp"));
        assert_eq!(instance.field("instance"), Some("NoSuchDiagnoseInstance"));
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.check == CheckId::InstanceResolution
                    && f.certainty == Certainty::Informational
                    && f.text.contains("tcp:host\\instance")),
            "{:#?}",
            report.findings
        );
        assert_eq!(
            checked(&report, CheckId::NameResolution).coverage,
            Coverage::NotApplicable
        );
        assert_eq!(report.exit_category(), ExitCategory::Inconclusive);
        assert_eq!(report.exit_code(), 2);

        // Through the stages too: none, stopped where asked.
        let report = observed(
            request("localhost\\SQLEXPRESS", Depth::EndpointResolution),
            Stages::new(),
            Ended::Stopped,
        );
        assert_eq!(
            checked(&report, CheckId::InstanceResolution).field("result"),
            Some("notLookedUp")
        );

        // A default instance, or a named one given with tcp:, is unchanged.
        let report = run(request(".", Depth::EndpointResolution));
        assert_eq!(
            checked(&report, CheckId::InstanceResolution).coverage,
            Coverage::NotApplicable
        );
        assert_eq!(report.exit_code(), 0, "{report:#?}");
        let report = observed(
            request("tcp:.\\SQLEXPRESS", Depth::EndpointResolution),
            Stages::new(),
            Ended::Stopped,
        );
        assert_ne!(
            checked(&report, CheckId::InstanceResolution).field("result"),
            Some("notLookedUp")
        );
    }

    /// A live default instance on this computer, when
    /// `MSSQL_SQLCMD_DIAGNOSTICS_SHARED_MEMORY` names it without a prefix,
    /// with a wrong password: SQL Server answered over shared memory, so the
    /// client tries no other transport and the diagnosis ends there.
    #[cfg(windows)]
    #[test]
    fn a_refused_login_over_shared_memory_ends_the_protocol_order() {
        let Ok(server) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_SHARED_MEMORY") else {
            return;
        };
        let mut live = request(&server, Depth::SessionValidation);
        live.authentication = Authentication::SqlPassword {
            user: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_USER").unwrap_or_default(),
            password: "not-the-password".to_string(),
        };
        let report = run(live);
        let attempts: Vec<&Check> = report
            .checks
            .iter()
            .filter(|c| c.id == CheckId::ConnectionAttempt)
            .collect();
        assert_eq!(attempts.len(), 1, "{:#?}", report.checks);
        assert_eq!(attempts[0].coverage, Coverage::Diagnosed);
        assert_eq!(attempts[0].field("transport"), Some("sharedMemory"));
        let tcp = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::TcpConnect)
            .unwrap();
        assert_eq!(tcp.coverage, Coverage::NotApplicable, "{tcp:#?}");
    }

    /// Blocking work the diagnosis started (a name lookup that does not
    /// return) does not hold up the report once the diagnosis ends.
    #[test]
    fn the_runtime_does_not_wait_for_blocking_work() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = Instant::now();
        block_on_and_leave(runtime, async {
            drop(tokio::task::spawn_blocking(|| {
                std::thread::sleep(Duration::from_secs(5));
            }));
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// The report keeps the request without its password, and no `Debug`
    /// output shows one.
    #[test]
    fn the_report_does_not_keep_the_password() {
        let mut with_password = request("tcp:db01,1433", Depth::ConnectionInput);
        with_password.authentication = Authentication::SqlPassword {
            user: "sa".to_string(),
            password: "secret-password".to_string(),
        };
        assert!(!format!("{with_password:?}").contains("secret-password"));
        let report = run(with_password);
        assert_eq!(
            report.request.authentication,
            Authentication::SqlPassword {
                user: "sa".to_string(),
                password: String::new(),
            }
        );
        assert!(!format!("{report:?}").contains("secret-password"));
    }

    /// A live default instance on this computer, when
    /// `MSSQL_SQLCMD_DIAGNOSTICS_SHARED_MEMORY` names it without a prefix
    /// (e.g. `localhost`), with the `..._USER` and `..._PASSWORD` of the live
    /// test: it connects over shared memory and the TCP checks do not apply.
    #[cfg(windows)]
    #[test]
    fn a_live_local_server_without_a_prefix_connects_over_shared_memory() {
        let Ok(server) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_SHARED_MEMORY") else {
            return;
        };
        let mut live = request(&server, Depth::SessionValidation);
        live.authentication = Authentication::SqlPassword {
            user: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_USER").unwrap_or_default(),
            password: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_PASSWORD").unwrap_or_default(),
        };
        let report = run(live);
        assert_eq!(report.outcome, DiagnosticOutcome::Passed, "{report:#?}");
        let attempt = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::ConnectionAttempt)
            .unwrap();
        assert_eq!(attempt.field("transport"), Some("sharedMemory"));
        for id in [
            CheckId::NameResolution,
            CheckId::InstanceResolution,
            CheckId::TcpConnect,
        ] {
            let check = report.checks.iter().find(|c| c.id == id).unwrap();
            assert_eq!(check.coverage, Coverage::NotApplicable, "{check:#?}");
        }
        let order: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
        assert_eq!(order, CheckId::ORDER);
    }

    #[test]
    fn a_live_ipv6_server_passes_with_and_without_a_prefix() {
        let Ok(server) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_IPV6") else {
            return;
        };
        for target in [server.clone(), format!("tcp:{server}")] {
            let mut live = request(&target, Depth::SessionValidation);
            live.authentication = Authentication::SqlPassword {
                user: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_USER").unwrap_or_default(),
                password: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_PASSWORD").unwrap_or_default(),
            };
            let report = run(live);
            assert_eq!(
                report.outcome,
                DiagnosticOutcome::Passed,
                "{target}: {report:#?}"
            );
        }
    }
    /// A live server, when `MSSQL_SQLCMD_DIAGNOSTICS_SERVER` names one (with
    /// `..._USER` and `..._PASSWORD`): every check passes, through session
    /// validation.
    #[test]
    fn a_live_server_passes_every_check() {
        let Ok(server) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_SERVER") else {
            return;
        };
        let mut live = request(&server, Depth::SessionValidation);
        live.authentication = Authentication::SqlPassword {
            user: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_USER").unwrap_or_default(),
            password: std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_PASSWORD").unwrap_or_default(),
        };
        live.login_timeout_seconds = 15;
        let report = run(live);
        assert_eq!(report.outcome, DiagnosticOutcome::Passed, "{report:#?}");
        let session = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::SessionValidation)
            .unwrap();
        assert_eq!(session.coverage, Coverage::Passed);
        assert!(report.server.as_ref().unwrap().target_type().is_some());
        assert_eq!(report.authentication, Some(AuthOutcome::Succeeded));
        let offset: i64 = session.field("clockOffsetMs").unwrap().parse().unwrap();
        assert!(offset.abs() < KERBEROS_CLOCK_TOLERANCE_MS, "{offset}");
        for id in [
            CheckId::NameResolution,
            CheckId::TcpConnect,
            CheckId::ConnectionAttempt,
            CheckId::SessionValidation,
        ] {
            let check = report.checks.iter().find(|c| c.id == id).unwrap();
            // An address literal needs no lookup, so has no time limit.
            let expected = (check.field("result") != Some("addressLiteral")).then_some(15_000);
            assert_eq!(check.deadline_ms, expected, "{id:?}");
        }
        assert_eq!(report.tracing_guide, None);
    }

    #[test]
    fn driver_tracing_is_suggested_only_where_the_client_sees_no_further() {
        let guide = |id: CheckId, coverage: Coverage| {
            let mut diagnosis = Diagnosis::new(request("tcp:db01,1433", Depth::SessionValidation));
            diagnosis.checks.push(Check::new(id, coverage));
            diagnosis.finish(0, 0).tracing_guide
        };
        for coverage in [Coverage::Inconclusive, Coverage::Classified] {
            assert_eq!(
                guide(CheckId::ConnectionAttempt, coverage),
                Some(TRACING_GUIDE)
            );
            assert_eq!(
                guide(CheckId::SessionValidation, coverage),
                Some(TRACING_GUIDE)
            );
        }
        assert_eq!(guide(CheckId::ConnectionAttempt, Coverage::Diagnosed), None);
        assert_eq!(guide(CheckId::ConnectionAttempt, Coverage::Passed), None);
        assert_eq!(guide(CheckId::TcpConnect, Coverage::Inconclusive), None);
    }

    #[test]
    fn only_a_diagnosed_check_makes_the_specialist_area_diagnosed() {
        let domain = |coverage: Coverage| {
            let mut diagnosis = Diagnosis::new(request("tcp:db01,1433", Depth::SessionValidation));
            let mut check = Check::new(CheckId::ConnectionAttempt, coverage);
            check.domain = Some(Domain::specific("security.authentication.sql"));
            diagnosis.checks.push(check);
            diagnosis.finish(0, 0).domain.unwrap()
        };
        assert!(domain(Coverage::Diagnosed).diagnosed);
        assert!(!domain(Coverage::Classified).diagnosed);
        assert!(!domain(Coverage::Inconclusive).diagnosed);
    }
    /// A live LocalDB instance, when `MSSQL_SQLCMD_DIAGNOSTICS_LOCALDB` names
    /// one: connected to as the client does (no encryption), it passes through
    /// session validation, the TCP checks not applying.
    #[cfg(windows)]
    #[test]
    fn a_live_localdb_instance_passes() {
        let Ok(instance) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_LOCALDB") else {
            return;
        };
        let mut localdb = request(&format!("(localdb)\\{instance}"), Depth::SessionValidation);
        localdb.authentication = Authentication::Integrated;
        let report = run(localdb);
        for check in &report.checks {
            assert!(
                matches!(check.coverage, Coverage::Passed | Coverage::NotApplicable),
                "{:?}: {report:#?}",
                check.id
            );
        }
        assert_eq!(report.exit_code(), 0, "{report:#?}");
    }
    /// The attempt ends at the check's time limit, whatever the client's own
    /// (whole-second, rounded-up) limits would allow.
    #[test]
    fn the_attempt_ends_at_the_time_limit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let started = Instant::now();
        let hung = runtime.block_on(bounded(
            Duration::from_millis(50),
            15,
            std::future::pending::<Result<(), Error>>(),
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(&hung, Err(Error::TimeoutError(_))), "{hung:?}");
        let done = runtime.block_on(bounded(Duration::from_secs(5), 15, async {
            Ok::<_, Error>(7)
        }));
        assert_eq!(done.unwrap(), 7);
    }
    /// LocalDB is resolved before the attempt (on a thread of its own); an
    /// instance that does not exist, or no LocalDB at all, fails the attempt.
    #[cfg(windows)]
    #[test]
    fn a_localdb_instance_that_does_not_exist_fails_the_attempt() {
        let report = run(request(
            "(localdb)\\NoSuchDiagnoseInstance",
            Depth::ConnectionAttempt,
        ));
        let attempt = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::ConnectionAttempt)
            .unwrap();
        assert_eq!(attempt.skip, None, "{report:#?}");
        assert_ne!(attempt.coverage, Coverage::Passed, "{report:#?}");
        assert!(!report.errors.is_empty());
    }

    /// A cancel counts only for the run in progress: one with no run is not
    /// kept, one during the run is never cleared by it, and none is carried
    /// into the next run.
    #[test]
    fn a_cancel_counts_only_for_the_run_in_progress() {
        let _only = RUNNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        end_run();
        assert!(
            !cancel_seen(STATE.load(Ordering::SeqCst)),
            "no run was in progress"
        );
        begin_run();
        assert!(!cancel_requested());
        cancel();
        assert!(cancel_requested(), "the run in progress is canceled");
        cancel();
        assert!(cancel_requested());
        end_run();
        assert!(!cancel_requested());
        // A cancel that read the state during one run, but lands after the
        // next has started, does not cancel the next.
        begin_run();
        let seen = STATE.load(Ordering::SeqCst);
        end_run();
        begin_run();
        assert!(!cancel_seen(seen), "a late cancel for an earlier run");
        assert!(!cancel_requested(), "not carried into the next run");
        end_run();
    }
    /// A canceled run keeps what finished, skips the rest as canceled, and
    /// reports itself canceled (exit code 4).
    #[test]
    fn a_canceled_run_reports_what_it_reached() {
        // A name that needs a lookup: the diagnosis is waiting when it is canceled.
        let report = run_until(
            request("tcp:canceled-run.invalid,1433", Depth::SessionValidation),
            &|| true,
        );
        assert_eq!(report.execution_status, ExecutionStatus::Canceled);
        assert_eq!(report.exit_category(), ExitCategory::Canceled);
        assert_eq!(report.exit_code(), 4);
        // Every check at the depth is accounted for, once.
        assert_eq!(report.checks.len(), CheckId::ORDER.len(), "{report:#?}");
        for id in CheckId::ORDER {
            let check = report.checks.iter().find(|c| c.id == id).unwrap();
            // What the client finished stands; what it did not is canceled.
            assert!(
                matches!(
                    check.skip,
                    None | Some(Skip::Canceled | Skip::NotApplicable(_))
                ),
                "{check:#?}"
            );
            assert!(!check.coverage.failed(), "{check:#?}");
        }
        assert!(
            report.checks.iter().any(|c| c.skip == Some(Skip::Canceled)),
            "{report:#?}"
        );
    }
    /// The dedicated administrator connection goes over TCP to port 1434, so
    /// its name resolution and TCP connect run against that port.
    #[test]
    fn the_admin_connection_is_checked_on_port_1434() {
        let report = run(request("admin:127.0.0.1", Depth::NetworkReachability));
        let check = |id: CheckId| report.checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(check(CheckId::NameResolution).coverage, Coverage::Passed);
        assert_eq!(
            check(CheckId::InstanceResolution).coverage,
            Coverage::NotApplicable
        );
        let tcp = check(CheckId::TcpConnect);
        assert_eq!(tcp.field("port"), Some("1434"), "{report:#?}");
        assert!(tcp.attempts.iter().all(|a| a.port == 1434));
    }
    /// A named instance whose port SQL Server Browser does not give: nothing
    /// that needs a port is tried, and the findings state evidence only, no
    /// advice (FR-012), but for the request for an explicit `host,port`
    /// (FR-067).
    #[test]
    fn a_named_instance_without_a_port_asks_only_for_the_port() {
        let report = run(request(
            "127.0.0.1\\NoSuchDiagnoseInstance",
            Depth::NetworkReachability,
        ));
        let check = |id: CheckId| report.checks.iter().find(|c| c.id == id).unwrap();
        let lookup = check(CheckId::InstanceResolution);
        assert_ne!(lookup.coverage, Coverage::Passed);
        assert_eq!(lookup.field(REQUIRES), Some(REQUIRES_EXPLICIT_PORT));
        assert_eq!(check(CheckId::TcpConnect).skip, Some(Skip::PortRequired));
        let (request, evidence): (Vec<&Finding>, Vec<&Finding>) = report
            .findings
            .iter()
            .partition(|f| f.text.starts_with("Give the instance's port explicitly"));
        assert_eq!(request.len(), 1, "{:#?}", report.findings);
        assert!(
            request[0].text.contains("-S 127.0.0.1,<port>"),
            "{}",
            request[0].text
        );
        assert!(!evidence.is_empty());
        for finding in evidence {
            for advice in ["give ", "explicit", "Verify", "Ensure", "should", "Contact"] {
                assert!(!finding.text.contains(advice), "{}", finding.text);
            }
        }
    }
    /// A named instance's DAC port comes from a Browser request the client
    /// does not make: no port is guessed, and nothing past it is checked.
    #[test]
    fn a_named_instance_admin_connection_is_not_guessed() {
        let report = run(request("admin:127.0.0.1\\INST", Depth::SessionValidation));
        let check = |id: CheckId| report.checks.iter().find(|c| c.id == id).unwrap();
        let lookup = check(CheckId::InstanceResolution);
        assert_eq!(lookup.coverage, Coverage::Inconclusive);
        assert_eq!(lookup.field("result"), Some("dacLookupUnsupported"));
        for id in [
            CheckId::TcpConnect,
            CheckId::ConnectionAttempt,
            CheckId::SessionValidation,
        ] {
            assert_eq!(
                check(id).skip,
                Some(Skip::BlockedBy(CheckId::InstanceResolution)),
                "{id:?}"
            );
        }
        assert!(report.limitations.iter().any(|l| l.contains("DAC request")));
        assert_eq!(report.exit_code(), 2, "{report:#?}");
    }
    /// Work that blocks (a slow DNS server, say) does not block the runtime:
    /// a cancel racing it is seen at once, and the work gets only its budget.
    #[test]
    fn a_slow_lookup_does_not_hold_up_a_cancel() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let slow = || std::thread::sleep(Duration::from_secs(3));
        let started = Instant::now();
        let canceled = runtime.block_on(async {
            tokio::select! {
                _ = within(Duration::from_secs(3), slow) => false,
                () = tokio::time::sleep(Duration::from_millis(50)) => true,
            }
        });
        assert!(canceled);
        assert!(started.elapsed() < Duration::from_secs(1));

        let started = Instant::now();
        assert_eq!(
            runtime.block_on(within(Duration::from_millis(50), slow)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            runtime.block_on(within(Duration::from_secs(5), || 7)),
            Some(7)
        );
    }
    #[test]
    fn a_ticket_cache_is_looked_for_where_kerberos_keeps_it() {
        let present = |path: &str| path == "/tmp/krb5cc_1000" || path == "/home/u/cc";
        assert_eq!(
            ticket_cache_state(None, 1000, present),
            ("found", "default".to_string())
        );
        assert_eq!(
            ticket_cache_state(Some(""), 1001, present),
            ("notFound", "default".to_string())
        );
        assert_eq!(
            ticket_cache_state(Some("FILE:/home/u/cc"), 0, present),
            ("found", "KRB5CCNAME".to_string())
        );
        assert_eq!(
            ticket_cache_state(Some("/home/u/other"), 0, present),
            ("notFound", "KRB5CCNAME".to_string())
        );
        assert_eq!(
            ticket_cache_state(Some("KCM:1000"), 0, present),
            ("unverified", "KCM".to_string())
        );
        assert_eq!(
            ticket_cache_state(Some("KEYRING:persistent:1000"), 0, present),
            ("unverified", "KEYRING".to_string())
        );
    }

    #[test]
    fn local_refusals_are_told_apart_from_network_failures() {
        let (local, remote) = if cfg!(windows) {
            ([10013, 10024, 10049, 10055], [10061, 10060, 10065, 10051])
        } else if cfg!(target_os = "macos") {
            ([1, 13, 49, 55], [61, 60, 65, 51])
        } else {
            ([1, 13, 99, 105], [111, 110, 113, 101])
        };
        for code in local {
            assert!(local_facility_error(code), "{code}");
        }
        for code in remote {
            assert!(!local_facility_error(code), "{code}");
        }
    }

    /// Windows will not open a TCP connection to the broadcast address
    /// (WSAEADDRNOTAVAIL): the check cannot complete, so the run is partial.
    #[cfg(windows)]
    #[test]
    fn a_connection_this_computer_refuses_makes_the_diagnosis_partial() {
        let report = run(request(
            "tcp:255.255.255.255,1433",
            Depth::SessionValidation,
        ));
        let tcp = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::TcpConnect)
            .unwrap();
        assert_eq!(tcp.coverage, Coverage::Inconclusive, "{report:#?}");
        assert_eq!(tcp.domain.as_ref().unwrap().primary, "client.platform");
        assert_eq!(report.execution_status, ExecutionStatus::Partial);
        assert_eq!(report.exit_category(), ExitCategory::Partial);
        assert_eq!(report.exit_code(), 3);
        assert!(
            report.limitations.iter().any(|l| l.contains("error 10049")),
            "{report:#?}"
        );
        let attempt = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::ConnectionAttempt)
            .unwrap();
        assert_eq!(attempt.coverage, Coverage::Skipped);
    }

    /// Connect stages, as the client records them, at offsets in ms.
    struct Stages {
        base: Instant,
        list: Vec<Step>,
    }

    impl Stages {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                list: Vec::new(),
            }
        }

        fn add(
            mut self,
            stage: Stage,
            ok: Option<bool>,
            from: u64,
            to: u64,
            details: &[(&'static str, &str)],
        ) -> Self {
            self.list.push(Step {
                stage,
                ok,
                duration_ms: to - from,
                details: details
                    .iter()
                    .map(|(k, v)| (*k, (*v).to_string()))
                    .collect(),
                started: self.base + Duration::from_millis(from),
                ended: self.base + Duration::from_millis(to),
            });
            self
        }
    }

    /// The report the checks give for `stages` and how the attempt `ended`,
    /// as [`Diagnosis::execute`] builds it, without a connection. The stages
    /// are a client's on Windows, which alone has pipes and shared memory, so
    /// the diagnosis models Windows whatever platform the test runs on.
    fn observed(request: Request, stages: Stages, ended: Ended) -> Report {
        observed_on(request, stages, ended, true)
    }

    /// [`observed`], modeling Windows (`windows`) or another platform.
    fn observed_on(request: Request, stages: Stages, ended: Ended, windows: bool) -> Report {
        observed_with(request, stages, ended, |diagnosis| {
            diagnosis.windows = windows
        })
    }

    /// [`observed`], with the diagnosis `prepare`d first.
    fn observed_with(
        request: Request,
        stages: Stages,
        ended: Ended,
        prepare: impl FnOnce(&mut Diagnosis),
    ) -> Report {
        let mut diagnosis = Diagnosis::new(request);
        diagnosis.windows = true;
        prepare(&mut diagnosis);
        let target = diagnosis.connection_input().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(diagnosis.observe(&target, stages.list, &ended, 10, None));
        if !matches!(ended, Ended::Connected) {
            diagnosis.skip_rest(
                CheckId::ConnectionAttempt,
                &Skip::BlockedBy(CheckId::ConnectionAttempt),
            );
        }
        diagnosis.finish(0, 0)
    }

    fn timed_out() -> Ended {
        Ended::Failed(Error::TimeoutError(
            mssql_tds::error::TimeoutErrorType::String("Timeout while connecting".to_string()),
        ))
    }

    fn checked(report: &Report, id: CheckId) -> &Check {
        report.checks.iter().find(|c| c.id == id).unwrap()
    }

    use mssql_tds::connection::connect_stage as cs;

    /// Every address shares the attempt's time limit: one that does not
    /// answer leaves the others untried, and nothing says the port is closed.
    #[test]
    fn the_time_limit_covers_every_address_together() {
        let mut timed = request("tcp:db01,1433", Depth::NetworkReachability);
        timed.login_timeout_seconds = 1;
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[
                    (cs::HOST, "db01"),
                    (cs::ADDRESSES, "192.0.2.1, 192.0.2.2, 192.0.2.3"),
                ],
            )
            // Abandoned: the time limit dropped it.
            .add(
                Stage::Tcp,
                None,
                1,
                1000,
                &[(cs::ADDRESS, "192.0.2.1"), (cs::PORT, "1433")],
            );
        let report = observed(timed, stages, timed_out());
        let check = checked(&report, CheckId::TcpConnect);
        assert_eq!(check.deadline_ms, Some(1000));
        let results: Vec<TcpResult> = check.attempts.iter().map(|a| a.result).collect();
        assert_eq!(
            results,
            [
                TcpResult::TimedOut,
                TcpResult::NotAttempted,
                TcpResult::NotAttempted
            ],
            "{check:#?}"
        );
        assert!(check.duration_ms.unwrap() < 1600);
        assert_eq!(check.coverage, Coverage::Inconclusive);
        // It timed out, so a normal run's timeout line is there.
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.code.as_deref() == Some("loginTimeoutExpired")),
            "{:#?}",
            report.errors
        );
    }

    /// A login the time limit ended after the transport opened, before the
    /// client had an operating-system error to report, still gets the
    /// driver's timeout line, as a normal sqlcmd run prints it.
    #[test]
    fn a_login_that_runs_out_of_time_reads_as_a_login_timeout() {
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                1,
                2,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            )
            .add(Stage::Prelogin, None, 2, 900, &[]);
        let report = observed(
            request("tcp:db01,1433", Depth::ConnectionAttempt),
            stages,
            timed_out(),
        );
        let codes: Vec<Option<&str>> = report.errors.iter().map(|e| e.code.as_deref()).collect();
        assert_eq!(
            codes,
            [Some("timedOut"), Some("loginTimeoutExpired")],
            "{:#?}",
            report.errors
        );
        assert!(
            report
                .errors
                .iter()
                .all(|e| e.check == CheckId::ConnectionAttempt)
        );
        assert_eq!(
            report.errors[1].text.as_deref(),
            Some("Login timeout expired")
        );
    }

    /// A stage the client abandoned (a time limit or a cancel dropped it) is
    /// not a failure: only a time limit makes it the phase that failed, and
    /// a name lookup it cut short is inconclusive, not diagnosed.
    #[test]
    fn an_abandoned_stage_is_not_a_failure() {
        let session = || {
            Stages::new()
                .add(
                    Stage::Dns,
                    Some(true),
                    0,
                    1,
                    &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
                )
                .add(
                    Stage::Tcp,
                    Some(true),
                    1,
                    2,
                    &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
                )
                .add(Stage::Prelogin, Some(true), 2, 3, &[])
                .add(Stage::Login, None, 3, 900, &[])
        };
        let protocol = Ended::Failed(Error::ProtocolError("unexpected token".to_string()));
        let report = observed(
            request("tcp:db01,1433", Depth::ConnectionAttempt),
            session(),
            protocol,
        );
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert_eq!(attempt.field("failedPhase"), None, "{attempt:#?}");
        // Nor is it shown as failed: the attempt ended elsewhere.
        let phase_status = |report: &Report| {
            let value: serde_json::Value =
                serde_json::from_str(&report::json(report, "v").unwrap()).unwrap();
            let check = value["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["check"] == "connectionAttempt")
                .unwrap()
                .clone();
            check["phases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["phase"] == "login")
                .unwrap()["status"]
                .clone()
        };
        assert_eq!(phase_status(&report), "abandoned");
        let text = report::text(&report);
        assert!(text.contains("login 897 ms ABANDONED"), "{text}");
        assert!(!text.contains("FAILED"), "{text}");

        let report = observed(
            request("tcp:db01,1433", Depth::ConnectionAttempt),
            session(),
            timed_out(),
        );
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert_eq!(attempt.field("failedPhase"), Some("login"), "{attempt:#?}");
        // The phase that ran out of time failed.
        assert_eq!(phase_status(&report), "failed");
        assert!(report::text(&report).contains("login 897 ms FAILED"));

        let cut_short = Stages::new().add(Stage::Dns, None, 0, 900, &[(cs::HOST, "db01")]);
        let report = observed(
            request("tcp:db01,1433", Depth::NetworkReachability),
            cut_short,
            timed_out(),
        );
        let name = checked(&report, CheckId::NameResolution);
        assert_eq!(name.coverage, Coverage::Inconclusive, "{name:#?}");
        assert_eq!(name.field("result"), Some("timedOut"));
        let failed = Stages::new().add(
            Stage::Dns,
            Some(false),
            0,
            5,
            &[
                (cs::HOST, "db01"),
                (cs::OS_ERROR, "11001"),
                (cs::ERROR, "No such host is known."),
            ],
        );
        let report = observed(
            request("tcp:db01,1433", Depth::NetworkReachability),
            failed,
            Ended::Failed(Error::Io(std::io::Error::from_raw_os_error(11001))),
        );
        let name = checked(&report, CheckId::NameResolution);
        assert_eq!(name.coverage, Coverage::Diagnosed, "{name:#?}");
        assert_eq!(name.field("result"), Some("notFound"));
        assert_eq!(
            checked(&report, CheckId::TcpConnect).skip,
            Some(Skip::BlockedBy(CheckId::NameResolution))
        );
    }

    /// The resolver failed and the client went on with addresses the DNS
    /// cache kept from an earlier resolution (the `dns` stage succeeds with
    /// `source=fallbackCache` and the resolver's error): name resolution is
    /// the resolver's failure, and the TCP connect is still checked with the
    /// cached addresses.
    #[test]
    fn addresses_from_the_fallback_cache_do_not_pass_name_resolution() {
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                5,
                &[
                    (cs::HOST, "db01"),
                    (cs::ADDRESSES, "10.0.0.1, 10.0.0.2"),
                    (cs::SOURCE, cs::SOURCE_FALLBACK_CACHE),
                    (cs::OS_ERROR, "11001"),
                    (cs::IO_KIND, "Uncategorized"),
                    (cs::ERROR, "No such host is known. (os error 11001)"),
                ],
            )
            .add(
                Stage::Tcp,
                Some(true),
                5,
                6,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            )
            .add(
                Stage::Tcp,
                Some(false),
                7,
                9,
                &[
                    (cs::ADDRESS, "10.0.0.2"),
                    (cs::PORT, "1433"),
                    (cs::OS_ERROR, "10061"),
                    (cs::IO_KIND, "ConnectionRefused"),
                    (cs::ERROR, "refused"),
                ],
            );
        let report = observed(
            request("tcp:db01,1433", Depth::NetworkReachability),
            stages,
            Ended::Stopped,
        );
        let name = checked(&report, CheckId::NameResolution);
        assert_eq!(name.coverage, Coverage::Diagnosed, "{name:#?}");
        assert_eq!(name.field("result"), Some("notFound"));
        assert_eq!(name.field("source"), Some("fallbackCache"));
        let addresses: Vec<&str> = name
            .fields
            .iter()
            .filter(|f| f.key == "addresses")
            .map(|f| f.value.as_str())
            .collect();
        assert_eq!(addresses, ["10.0.0.1", "10.0.0.2"]);
        let record = report
            .errors
            .iter()
            .find(|e| e.check == CheckId::NameResolution)
            .unwrap();
        assert_eq!(record.os_error, Some(11001));
        assert_eq!(record.code.as_deref(), Some("notFound"));
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.check == CheckId::NameResolution
                    && f.certainty == Certainty::Informational
                    && f.text.contains("cached from an earlier resolution")),
            "{:#?}",
            report.findings
        );
        // The TCP connect is not blocked: each cached address is reported.
        let tcp = checked(&report, CheckId::TcpConnect);
        assert_eq!(tcp.skip, None, "{tcp:#?}");
        let results: Vec<TcpResult> = tcp.attempts.iter().map(|a| a.result).collect();
        assert_eq!(
            results,
            [TcpResult::Connected, TcpResult::Refused],
            "{tcp:#?}"
        );
        assert_eq!(report.outcome, DiagnosticOutcome::IssueDetected);
        assert_eq!(report.exit_code(), 1);
    }

    /// The addresses after the one the connection used are tried too: each
    /// is reported, and the check passes with the one that accepted.
    #[test]
    fn the_addresses_after_the_one_used_are_reported() {
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1, 10.0.0.2")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                1,
                2,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            )
            .add(
                Stage::Tcp,
                Some(false),
                3,
                9,
                &[
                    (cs::ADDRESS, "10.0.0.2"),
                    (cs::PORT, "1433"),
                    (cs::IO_KIND, "ConnectionRefused"),
                    (cs::OS_ERROR, "10061"),
                    (cs::ERROR, "refused"),
                ],
            );
        let report = observed(
            request("tcp:db01,1433", Depth::NetworkReachability),
            stages,
            Ended::Stopped,
        );
        let check = checked(&report, CheckId::TcpConnect);
        assert_eq!(check.coverage, Coverage::Passed);
        let attempts: Vec<(&str, TcpResult)> = check
            .attempts
            .iter()
            .map(|a| (a.address.as_str(), a.result))
            .collect();
        assert_eq!(
            attempts,
            [
                ("10.0.0.1", TcpResult::Connected),
                ("10.0.0.2", TcpResult::Refused)
            ]
        );
        assert_eq!(check.attempts[1].os_error, Some(10061));
        assert_eq!(report.outcome, DiagnosticOutcome::Passed);
        let ids: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            [
                CheckId::ConnectionInput,
                CheckId::NameResolution,
                CheckId::InstanceResolution,
                CheckId::TcpConnect
            ],
            "the stop leaves nothing deeper"
        );
    }

    #[test]
    fn a_link_local_address_keeps_its_scope() {
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "fe80::1%12, fe80::1%7")],
            )
            .add(
                Stage::Tcp,
                Some(false),
                1,
                2,
                &[
                    (cs::ADDRESS, "fe80::1%12"),
                    (cs::PORT, "1433"),
                    (cs::IO_KIND, "ConnectionRefused"),
                ],
            )
            .add(
                Stage::Tcp,
                Some(true),
                2,
                3,
                &[(cs::ADDRESS, "fe80::1%7"), (cs::PORT, "1433")],
            );
        let report = observed(
            request("tcp:db01,1433", Depth::NetworkReachability),
            stages,
            Ended::Stopped,
        );
        let name = checked(&report, CheckId::NameResolution);
        let listed: Vec<&str> = name
            .fields
            .iter()
            .filter(|f| f.key == "addresses")
            .map(|f| f.value.as_str())
            .collect();
        assert_eq!(listed, ["fe80::1%12", "fe80::1%7"]);
        let shown: Vec<(&str, u16)> = checked(&report, CheckId::TcpConnect)
            .attempts
            .iter()
            .map(|a| (a.address.as_str(), a.port))
            .collect();
        assert_eq!(shown, [("fe80::1%12", 1433), ("fe80::1%7", 1433)]);
        let refused = report.errors.iter().find(|e| e.source == "os").unwrap();
        assert!(
            refused.message.starts_with("fe80::1%12:1433: "),
            "{refused:#?}"
        );
    }

    /// A login the gateway redirected: the TCP connect reported is the
    /// target's, and the phases are those of the connection the client made
    /// where it was sent.
    #[test]
    fn a_redirect_reports_the_connection_after_it() {
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                1,
                2,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            )
            .add(Stage::Prelogin, Some(true), 2, 3, &[])
            .add(Stage::Tls, Some(true), 4, 5, &[])
            .add(Stage::Login, None, 3, 6, &[(cs::REDIRECTED, "true")])
            .add(
                Stage::Dns,
                Some(true),
                7,
                8,
                &[(cs::HOST, "node7"), (cs::ADDRESSES, "10.0.0.9")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                8,
                9,
                &[(cs::ADDRESS, "10.0.0.9"), (cs::PORT, "11000")],
            )
            .add(Stage::Prelogin, Some(true), 9, 10, &[])
            .add(Stage::Tls, Some(true), 11, 12, &[])
            .add(Stage::Login, Some(true), 10, 20, &[]);
        let report = observed(
            request("tcp:db01,1433", Depth::ConnectionAttempt),
            stages,
            Ended::Connected,
        );
        let tcp = checked(&report, CheckId::TcpConnect);
        assert_eq!(tcp.attempts.len(), 1);
        assert_eq!(tcp.attempts[0].address, "10.0.0.1");
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert_eq!(attempt.coverage, Coverage::Passed);
        assert_eq!(attempt.field("redirected"), Some("true"));
        assert_eq!(attempt.subphases.len(), 3, "{attempt:#?}");
        assert!(attempt.subphases.iter().all(|s| s.ok == Some(true)));
        assert_eq!(
            checked(&report, CheckId::NameResolution).field("host"),
            Some("db01")
        );
    }

    /// Without a prefix the client's protocol order moves on: the transports
    /// it moved past are evidence, marked superseded, and the attempt is the
    /// last transport's. A TCP path the pipe that opened replaced does not
    /// apply.
    #[test]
    fn the_protocol_order_is_reported_as_the_client_followed_it() {
        // A name that does not resolve, then the server's standard pipe.
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(false),
                0,
                5,
                &[
                    (cs::HOST, "db01"),
                    (cs::OS_ERROR, "11001"),
                    (cs::ERROR, "No such host is known."),
                ],
            )
            .add(
                Stage::Pipe,
                Some(true),
                6,
                7,
                &[
                    (cs::KIND, cs::KIND_NAMED_PIPE),
                    (cs::PIPE_PATH, r"\\db01\pipe\sql\query"),
                ],
            )
            .add(Stage::Prelogin, Some(true), 7, 8, &[])
            .add(Stage::Login, Some(true), 8, 9, &[]);
        let report = observed(
            request("db01", Depth::ConnectionAttempt),
            stages,
            Ended::Connected,
        );
        for id in [CheckId::NameResolution, CheckId::TcpConnect] {
            assert_eq!(
                checked(&report, id).skip,
                Some(Skip::NotApplicable(NOT_ON_THE_PATH)),
                "{id:?}"
            );
        }
        assert_eq!(report.outcome, DiagnosticOutcome::Passed);
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert_eq!(attempt.coverage, Coverage::Passed);
        assert_eq!(attempt.field("transport"), Some("namedPipe"));
        assert_eq!(attempt.field("pipe"), Some(r"\\db01\pipe\sql\query"));
        assert!(
            report.errors.iter().all(|e| e.superseded),
            "{:#?}",
            report.errors
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.text.starts_with("TCP did not connect")),
            "{:#?}",
            report.findings
        );

        // Shared memory first, which did not open, then TCP.
        let stages = Stages::new()
            .add(
                Stage::Pipe,
                Some(false),
                0,
                1,
                &[
                    (cs::KIND, cs::KIND_SHARED_MEMORY),
                    (cs::PIPE_PATH, r"\\.\pipe\SQLLocal\MSSQLSERVER"),
                    (cs::OS_ERROR, "2"),
                    (cs::ERROR, "The system cannot find the file specified."),
                ],
            )
            .add(
                Stage::Dns,
                Some(true),
                2,
                3,
                &[(cs::HOST, "localhost"), (cs::ADDRESSES, "127.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                3,
                4,
                &[(cs::ADDRESS, "127.0.0.1"), (cs::PORT, "1433")],
            )
            .add(Stage::Prelogin, Some(true), 4, 5, &[])
            .add(Stage::Login, Some(true), 5, 6, &[]);
        let report = observed(
            request("localhost", Depth::ConnectionAttempt),
            stages,
            Ended::Connected,
        );
        assert!(
            report.findings.iter().any(|f| f
                .text
                .starts_with("Shared memory to this computer did not open")),
            "{:#?}",
            report.findings
        );
        let set_aside = report
            .errors
            .iter()
            .find(|e| e.os_error == Some(2))
            .unwrap();
        assert!(set_aside.superseded && set_aside.text.is_none());
        assert_eq!(
            checked(&report, CheckId::TcpConnect).coverage,
            Coverage::Passed
        );
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert_eq!(attempt.field("transport"), None, "{attempt:#?}");
        assert_eq!(report.outcome, DiagnosticOutcome::Passed);

        // Shared memory that answered ends the protocol order there.
        let stages = Stages::new()
            .add(
                Stage::Pipe,
                Some(true),
                0,
                1,
                &[
                    (cs::KIND, cs::KIND_SHARED_MEMORY),
                    (cs::PIPE_PATH, r"\\.\pipe\SQLLocal\MSSQLSERVER"),
                ],
            )
            .add(Stage::Prelogin, Some(true), 1, 2, &[])
            .add(Stage::Login, Some(false), 2, 3, &[]);
        let report = observed(
            request("localhost", Depth::ConnectionAttempt),
            stages,
            Ended::Failed(server_error(18456, "Login failed for user 'sa'.")),
        );
        for id in [
            CheckId::NameResolution,
            CheckId::InstanceResolution,
            CheckId::TcpConnect,
        ] {
            assert_eq!(
                checked(&report, id).skip,
                Some(Skip::NotApplicable(SHARED_MEMORY_FIRST)),
                "{id:?}"
            );
        }
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert_eq!(attempt.coverage, Coverage::Diagnosed);
        assert_eq!(attempt.field("transport"), Some("sharedMemory"));
        assert_eq!(attempt.field("failedPhase"), Some("login"));
    }

    /// The SQL Server Browser lookup is read from its stage: its port, or why
    /// it gave none, with the checks that need a port skipped.
    #[test]
    fn the_instance_lookup_is_read_from_its_stage() {
        let answered = Stages::new()
            .add(
                Stage::InstanceLookup,
                Some(true),
                0,
                3,
                &[
                    (cs::SERVER, "db01"),
                    (cs::INSTANCE, "SQL2022"),
                    (cs::ADDRESSES, "10.0.0.1"),
                    (cs::PORT, "50001"),
                ],
            )
            .add(
                Stage::Dns,
                Some(true),
                3,
                4,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            );
        let report = observed(
            request(r"db01\SQL2022", Depth::EndpointResolution),
            answered,
            Ended::Stopped,
        );
        let lookup = checked(&report, CheckId::InstanceResolution);
        assert_eq!(lookup.coverage, Coverage::Passed);
        assert_eq!(lookup.field("port"), Some("50001"));
        assert_eq!(lookup.deadline_ms, Some(BROWSER_TIMEOUT_MS));

        let unusable = Stages::new().add(
            Stage::InstanceLookup,
            Some(false),
            0,
            3,
            &[
                (cs::SERVER, "db01"),
                (cs::INSTANCE, "SQL2022"),
                (cs::ADDRESSES, "10.0.0.1"),
                (cs::RESULT, cs::NO_ENDPOINT),
            ],
        );
        let report = observed(
            request(r"db01\SQL2022", Depth::SessionValidation),
            unusable,
            Ended::Failed(Error::ConnectionError("no endpoint".to_string())),
        );
        let lookup = checked(&report, CheckId::InstanceResolution);
        assert_eq!(lookup.coverage, Coverage::Diagnosed);
        assert_eq!(lookup.field("result"), Some("noTcpPort"));
        assert_eq!(
            checked(&report, CheckId::NameResolution).coverage,
            Coverage::Passed
        );
        for id in [CheckId::TcpConnect, CheckId::ConnectionAttempt] {
            assert_eq!(
                checked(&report, id).skip,
                Some(Skip::PortRequired),
                "{id:?}"
            );
        }
        // One failure, reported once: by the lookup, not the attempt too.
        let failing: Vec<CheckId> = report
            .errors
            .iter()
            .filter(|e| e.source != "client")
            .map(|e| e.check)
            .collect();
        assert_eq!(
            failing,
            [CheckId::InstanceResolution],
            "{:#?}",
            report.errors
        );

        let unresolved = Stages::new().add(
            Stage::InstanceLookup,
            Some(false),
            0,
            3,
            &[
                (cs::SERVER, "db01"),
                (cs::INSTANCE, "SQL2022"),
                (cs::RESULT, "nameResolutionFailed"),
                (cs::OS_ERROR, "11001"),
                (cs::ERROR, "No such host is known."),
            ],
        );
        let report = observed(
            request(r"db01\SQL2022", Depth::NetworkReachability),
            unresolved,
            Ended::Failed(Error::ConnectionError("x".to_string())),
        );
        assert_eq!(
            checked(&report, CheckId::NameResolution).field("result"),
            Some("notFound")
        );
        assert_eq!(
            checked(&report, CheckId::InstanceResolution).skip,
            Some(Skip::BlockedBy(CheckId::NameResolution))
        );
        assert_eq!(
            checked(&report, CheckId::TcpConnect).skip,
            Some(Skip::BlockedBy(CheckId::NameResolution))
        );
    }

    /// SQL Server Browser answered for `db01\SQL2022` with no TCP port.
    fn no_port_stages(result: &str) -> Stages {
        Stages::new().add(
            Stage::InstanceLookup,
            Some(false),
            0,
            3,
            &[
                (cs::SERVER, "db01"),
                (cs::INSTANCE, "SQL2022"),
                (cs::ADDRESSES, "10.0.0.1"),
                (cs::RESULT, result),
            ],
        )
    }

    /// The Browser lookup's resolving used up its time limit before SQL
    /// Server Browser was asked (`timedOut`, no addresses; a silent Browser is
    /// `noAnswer`): name resolution timed out, within the lookup's limit, and
    /// instance resolution is blocked by it, asking for no port.
    #[test]
    fn a_lookup_whose_resolving_timed_out_is_a_name_resolution_timeout() {
        let stages = || {
            Stages::new().add(
                Stage::InstanceLookup,
                Some(false),
                0,
                2000,
                &[
                    (cs::SERVER, "db01"),
                    (cs::INSTANCE, "SQL2022"),
                    (cs::RESULT, "timedOut"),
                    (
                        cs::ERROR,
                        "Resolving 'db01' for the SQL Server Browser lookup did not complete \
                         within 2000ms.",
                    ),
                ],
            )
        };
        for depth in [Depth::EndpointResolution, Depth::NetworkReachability] {
            let report = observed(
                request("db01\\SQL2022", depth),
                stages(),
                Ended::Failed(Error::ConnectionError("timed out".to_string())),
            );
            let name = checked(&report, CheckId::NameResolution);
            assert_eq!(name.coverage, Coverage::Inconclusive, "{name:#?}");
            assert_eq!(name.field("result"), Some("timedOut"));
            assert_eq!(name.field("host"), Some("db01"));
            assert_eq!(name.deadline_ms, Some(BROWSER_TIMEOUT_MS));
            assert_eq!(
                checked(&report, CheckId::InstanceResolution).skip,
                Some(Skip::BlockedBy(CheckId::NameResolution))
            );
            assert!(
                report
                    .checks
                    .iter()
                    .all(|c| c.field(REQUIRES).is_none() && c.skip != Some(Skip::PortRequired)),
                "{:#?}",
                report.checks
            );
            assert!(
                report.findings.iter().all(|f| f.text != PORT_REQUEST),
                "{:#?}",
                report.findings
            );
            assert!(
                report.findings.iter().all(|f| !f.text.contains("2000ms..")),
                "{:#?}",
                report.findings
            );
            assert_eq!(report.exit_code(), 2, "{depth:?}");
        }
    }

    const PORT_REQUEST: &str = "Give the instance's port explicitly, as -S db01,<port>: sqlcmd \
                                does not try port 1433 or any other port for a named instance \
                                whose port SQL Server Browser did not give.";

    /// The request for an explicit `host,port` is on instance resolution
    /// itself, in the text and JSON reports, with the host and instance shown
    /// as they are.
    fn assert_port_requested(report: &Report) {
        let lookup = checked(report, CheckId::InstanceResolution);
        assert_eq!(lookup.field(REQUIRES), Some(REQUIRES_EXPLICIT_PORT));
        let requests: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.text == PORT_REQUEST)
            .collect();
        assert_eq!(requests.len(), 1, "{:#?}", report.findings);
        assert_eq!(requests[0].check, CheckId::InstanceResolution);
        assert_eq!(requests[0].certainty, Certainty::Informational);

        let text = report::text(report);
        assert!(
            text.contains(
                "  Instance resolution   DIAGNOSED         3 ms  db01\\SQL2022: no usable port; needs an explicit host,port\n"
            ) || text.contains(
                "  Instance resolution   INCONCLUSIVE      3 ms  db01\\SQL2022: no usable port; needs an explicit host,port\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "[informational] Give the instance's port explicitly, as -S db01,<port>: sqlcmd \
                 does not try port 1433"
            ),
            "{text}"
        );
        let json = report::json(report, "v").unwrap();
        assert!(json.contains("\"requires\": \"explicitPort\""), "{json}");
        assert!(
            json.contains("\"text\": \"Give the instance's port explicitly, as -S db01,<port>:"),
            "{json}"
        );
    }

    /// At `endpointResolution` no check that needs a port is in scope, and
    /// instance resolution still asks for the port.
    #[test]
    fn a_named_instance_without_a_port_asks_for_it_at_endpoint_resolution() {
        let report = observed(
            request(r"db01\SQL2022", Depth::EndpointResolution),
            no_port_stages(cs::NO_ENDPOINT),
            Ended::Stopped,
        );
        assert_port_requested(&report);
        let ids: Vec<CheckId> = report.checks.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            [
                CheckId::ConnectionInput,
                CheckId::NameResolution,
                CheckId::InstanceResolution
            ]
        );
        let json = report::json(&report, "v").unwrap();
        assert!(!json.contains("explicitPortRequired"), "{json}");

        let mut local = request(r"db01\SQL2022", Depth::EndpointResolution);
        local.local_detail = true;
        let report = observed(local, no_port_stages(cs::NO_ENDPOINT), Ended::Stopped);
        let text = report::text(&report);
        assert!(text.contains("as -S db01,<port>:"), "{text}");
    }

    /// At the default depth the checks that need the port are skipped,
    /// blocked by instance resolution, which asks for it.
    #[test]
    fn a_named_instance_without_a_port_blocks_the_deeper_checks() {
        let report = observed(
            request(r"db01\SQL2022", Depth::SessionValidation),
            no_port_stages(cs::NO_ENDPOINT),
            Ended::Failed(Error::ConnectionError("no endpoint".to_string())),
        );
        assert_port_requested(&report);
        let text = report::text(&report);
        let json = report::json(&report, "v").unwrap();
        for id in [
            CheckId::TcpConnect,
            CheckId::ConnectionAttempt,
            CheckId::SessionValidation,
        ] {
            assert_eq!(
                checked(&report, id).skip,
                Some(Skip::PortRequired),
                "{id:?}"
            );
            let line = format!(
                "  {:<20}  SKIPPED                 blocked by Instance resolution: give an \
                 explicit host,port for the named instance\n",
                id.title()
            );
            assert!(text.contains(&line), "{line}{text}");
            let entry = format!(
                "{{ \"check\": \"{}\", \"depth\": \"{}\", \"status\": \"skipped\", \
                 \"reason\": \"explicitPortRequired\", \"blockedBy\": \"instanceResolution\", \
                 \"detail\": \"blocked by Instance resolution: give an explicit host,port for \
                 the named instance\" }}",
                id.name(),
                id.depth().name()
            );
            assert!(json.contains(&entry), "{entry}\n{json}");
        }
    }

    /// When this computer would not let the Browser request out, the run is
    /// partial, and instance resolution still asks for the port it blocks on.
    #[test]
    fn a_browser_request_refused_here_still_asks_for_the_port() {
        let report = observed(
            request(r"db01\SQL2022", Depth::SessionValidation),
            no_port_stages("socketFailed"),
            Ended::Failed(Error::ConnectionError("no socket".to_string())),
        );
        assert_eq!(report.execution_status, ExecutionStatus::Partial);
        assert_eq!(
            checked(&report, CheckId::InstanceResolution).coverage,
            Coverage::Inconclusive
        );
        assert_port_requested(&report);
        assert_eq!(
            checked(&report, CheckId::TcpConnect).skip,
            Some(Skip::PortRequired)
        );
    }

    /// `np:host\instance` with no pipe from the Browser: the checks after it
    /// are blocked by it, and no port is asked for.
    #[test]
    fn a_pipe_instance_without_a_pipe_asks_for_no_port() {
        let report = observed(
            request(r"np:db01\SQL2022", Depth::SessionValidation),
            no_port_stages("noAnswer"),
            Ended::Failed(Error::ConnectionError("no answer".to_string())),
        );
        let lookup = checked(&report, CheckId::InstanceResolution);
        assert_ne!(lookup.coverage, Coverage::Passed);
        assert_eq!(lookup.field(REQUIRES), None);
        assert!(
            report
                .findings
                .iter()
                .all(|f| !f.text.contains("port explicitly")),
            "{:#?}",
            report.findings
        );
        for id in [CheckId::ConnectionAttempt, CheckId::SessionValidation] {
            assert_eq!(
                checked(&report, id).skip,
                Some(Skip::BlockedBy(CheckId::InstanceResolution)),
                "{id:?}"
            );
        }
        let json = report::json(&report, "v").unwrap();
        assert!(!json.contains("explicitPort"), "{json}");
    }

    /// The text report says what the Browser did not give a pipe instance:
    /// its pipe, not a port.
    #[test]
    fn a_pipe_instance_without_a_pipe_reads_as_no_usable_pipe() {
        let report = observed(
            request(r"np:db01\SQL2022", Depth::SessionValidation),
            no_port_stages("noAnswer"),
            Ended::Failed(Error::ConnectionError("no answer".to_string())),
        );
        let text = report::text(&report);
        assert!(
            text.contains(
                "  Instance resolution   INCONCLUSIVE      3 ms  db01\\SQL2022: no usable pipe\n"
            ),
            "{text}"
        );
        assert!(!text.contains("no usable port"), "{text}");
    }

    /// The SPN reported is the one the client recorded requesting.
    #[test]
    fn the_reported_spn_is_the_one_the_client_requested() {
        let mut integrated = request("tcp:db01,1433", Depth::ConnectionAttempt);
        integrated.authentication = Authentication::Integrated;
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                1,
                2,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            )
            .add(Stage::Prelogin, Some(true), 2, 3, &[])
            .add(
                Stage::Login,
                Some(false),
                3,
                4,
                &[(cs::SPN, "MSSQLSvc/db01.contoso.com:1433")],
            );
        let report = observed(
            integrated,
            stages,
            Ended::Failed(server_error(18456, "Login failed.")),
        );
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert_eq!(attempt.field("spn"), Some("MSSQLSvc/db01.contoso.com:1433"));
    }

    /// A login with integrated authentication that the server refused,
    /// replayed with the ticket cache lookup giving `state` (looked for in
    /// `KRB5CCNAME`), failing in `failed` (the login, or before it).
    fn integrated_login_refused(state: &'static str, failed: Stage) -> Report {
        let mut integrated = request("tcp:db01,1433", Depth::ConnectionAttempt);
        integrated.authentication = Authentication::Integrated;
        let mut stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                1,
                2,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            );
        stages = if failed == Stage::Login {
            stages.add(Stage::Prelogin, Some(true), 2, 3, &[]).add(
                Stage::Login,
                Some(false),
                3,
                4,
                &[],
            )
        } else {
            stages.add(failed, Some(false), 2, 3, &[])
        };
        let lookup: fn() -> (&'static str, String) = match state {
            "notFound" => || ("notFound", "KRB5CCNAME".to_string()),
            _ => || ("found", "KRB5CCNAME".to_string()),
        };
        observed_with(
            integrated,
            stages,
            Ended::Failed(server_error(18456, "Login failed.")),
            |diagnosis| diagnosis.ticket_cache = Some(lookup),
        )
    }

    /// A login refused with integrated authentication, and no ticket cache
    /// where sqlcmd looked: the connection attempt says so. With a cache, or
    /// a failure before the login, it does not.
    #[test]
    fn a_refused_integrated_login_without_a_ticket_cache_is_suspected() {
        let suspected = no_ticket_cache_finding(Some("KRB5CCNAME"));
        let claims = |report: &Report| {
            report.findings.iter().any(|f| {
                f.check == CheckId::ConnectionAttempt
                    && f.certainty == Certainty::Suspected
                    && f.text == suspected
            })
        };
        let report = integrated_login_refused("notFound", Stage::Login);
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert_eq!(attempt.field("kerberosTicketCache"), Some("notFound"));
        assert_eq!(attempt.field("failedPhase"), Some("login"));
        assert!(claims(&report), "{:#?}", report.findings);

        let report = integrated_login_refused("found", Stage::Login);
        assert_eq!(
            checked(&report, CheckId::ConnectionAttempt).field("kerberosTicketCache"),
            Some("found")
        );
        assert!(!claims(&report), "{:#?}", report.findings);

        let report = integrated_login_refused("notFound", Stage::Prelogin);
        assert_ne!(
            checked(&report, CheckId::ConnectionAttempt).field("failedPhase"),
            Some("login")
        );
        assert!(!claims(&report), "{:#?}", report.findings);
    }

    /// The session's clock offset beyond what Kerberos tolerates by default
    /// is noted; within it, nothing is.
    #[test]
    fn a_clock_offset_beyond_the_kerberos_tolerance_is_noted() {
        let sent = 1_800_000_000_000;
        let received = sent + 20;
        let middle = sent + 10;
        for (offset, noted) in [
            (KERBEROS_CLOCK_TOLERANCE_MS + 1_000, Some("ahead of")),
            (-(KERBEROS_CLOCK_TOLERANCE_MS + 60_000), Some("behind")),
            (KERBEROS_CLOCK_TOLERANCE_MS, None),
            (-KERBEROS_CLOCK_TOLERANCE_MS + 1, None),
            (0, None),
        ] {
            let mut diagnosis = Diagnosis::new(request("tcp:db01,1433", Depth::SessionValidation));
            let probe = SessionProbe {
                engine_edition: 2,
                server_unix_ms: Some(middle + offset),
            };
            diagnosis.session_probed(
                Check::new(CheckId::SessionValidation, Coverage::Passed),
                &probe,
                sent,
                received,
            );
            let report = diagnosis.finish(0, 0);
            let check = checked(&report, CheckId::SessionValidation);
            assert_eq!(check.coverage, Coverage::Passed);
            let expected_offset = offset.to_string();
            assert_eq!(check.field("clockOffsetMs"), Some(expected_offset.as_str()));
            let notes: Vec<&Finding> = report
                .findings
                .iter()
                .filter(|f| f.check == CheckId::SessionValidation)
                .collect();
            match noted {
                Some(direction) => {
                    assert_eq!(notes.len(), 1, "{offset}: {notes:#?}");
                    assert_eq!(notes[0].certainty, Certainty::Informational);
                    let seconds = offset.abs() / 1000;
                    assert_eq!(
                        notes[0].text,
                        format!(
                            "The server's clock is {seconds} s {direction} the client's, more \
                             than the 5 minutes Kerberos tolerates by default."
                        )
                    );
                }
                None => assert!(notes.is_empty(), "{offset}: {notes:#?}"),
            }
        }
    }

    /// Every login error the server ends a login with is an authentication
    /// failure, in the area that owns the authentication used; every
    /// database error is the server's, after a good login.
    #[test]
    fn every_login_and_database_error_is_classified() {
        let sql = Authentication::SqlPassword {
            user: "sa".to_string(),
            password: String::new(),
        };
        let integrated_area = if cfg!(windows) {
            "security.authentication.windows"
        } else {
            "security.authentication.kerberos"
        };
        assert_eq!(LOGIN_ERRORS, [18456, 18452, 18470, 18486, 18487, 18488]);
        assert_eq!(DATABASE_ERRORS, [4060, 4064]);
        for number in LOGIN_ERRORS {
            for (authentication, area) in [
                (&sql, "security.authentication.sql"),
                (&Authentication::Integrated, integrated_area),
            ] {
                let classified = classify_connection_error(
                    &server_error(number, "Login failed."),
                    Some(Stage::Login),
                    authentication,
                );
                assert_eq!(classified.coverage, Coverage::Diagnosed, "{number}");
                assert_eq!(classified.domain, Domain::specific(area), "{number}");
                assert_eq!(classified.auth, AuthOutcome::Failed, "{number}");
            }
        }
        for number in DATABASE_ERRORS {
            let classified = classify_connection_error(
                &server_error(number, "Cannot open database."),
                Some(Stage::Login),
                &sql,
            );
            assert_eq!(classified.coverage, Coverage::Diagnosed, "{number}");
            assert_eq!(
                classified.domain,
                Domain::specific("target.sqlServer"),
                "{number}"
            );
            assert_eq!(classified.auth, AuthOutcome::Succeeded, "{number}");
        }
        // Any other server error is not taken for either.
        let other = classify_connection_error(&server_error(50000, "x"), Some(Stage::Login), &sql);
        assert_eq!(
            other.domain,
            Domain::undetermined(&["security.authentication.sql", "target.sqlServer"])
        );
    }

    /// The connection is one attempt, without a retry, in the time limit;
    /// SQL Server Browser has its own.
    #[test]
    fn the_client_makes_one_attempt_in_the_time_limit() {
        let diagnosis = Diagnosis::new(request("tcp:db01,1433", Depth::ConnectionAttempt));
        let context = diagnosis.client_context();
        assert_eq!(context.connect_retry_count, 0);
        assert_eq!(context.connect_timeout, 3);
        assert_eq!(context.login_timeout, Some(3));
        assert_eq!(context.ssrp_timeout_ms, BROWSER_TIMEOUT_MS);
    }

    /// The report of a run canceled after the client recorded `stages`, on
    /// Windows, as [`observed`].
    fn canceled_with(request: Request, stages: Stages) -> Report {
        let mut diagnosis = Diagnosis::new(request);
        diagnosis.windows = true;
        diagnosis.connection_input().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(diagnosis.canceled_after(stages.list));
        diagnosis.finish(0, 0)
    }

    /// The cancel decides the status: a local TCP failure replayed from the
    /// stages before it (which makes a run partial) does not downgrade it.
    #[test]
    fn a_cancel_stays_canceled_after_a_local_failure() {
        let local = if cfg!(windows) { "10024" } else { "24" };
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(false),
                1,
                2,
                &[
                    (cs::ADDRESS, "10.0.0.1"),
                    (cs::PORT, "1433"),
                    (cs::OS_ERROR, local),
                    (cs::ERROR, "too many open files"),
                ],
            )
            .add(
                Stage::Pipe,
                None,
                2,
                900,
                &[
                    (cs::KIND, cs::KIND_NAMED_PIPE),
                    (cs::PIPE_PATH, r"\\db01\pipe\sql\query"),
                ],
            );
        for depth in [Depth::NetworkReachability, Depth::SessionValidation] {
            let report = canceled_with(request("db01", depth), stages_clone(&stages));
            assert_eq!(
                report.execution_status,
                ExecutionStatus::Canceled,
                "{depth:?}: {report:#?}"
            );
            assert_eq!(report.exit_code(), 4, "{depth:?}");
        }
    }

    fn stages_clone(stages: &Stages) -> Stages {
        Stages {
            base: stages.base,
            list: stages.list.clone(),
        }
    }

    /// A cancel keeps the checks the client finished: the stages it recorded
    /// are read as after a normal attempt, and those it abandoned are skipped
    /// as canceled, not failed, with no error.
    #[test]
    fn a_cancel_keeps_the_stages_the_client_finished() {
        let connected = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1, 10.0.0.2")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                1,
                2,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            )
            .add(
                Stage::Tcp,
                None,
                2,
                900,
                &[(cs::ADDRESS, "10.0.0.2"), (cs::PORT, "1433")],
            );
        let report = canceled_with(
            request("tcp:db01,1433", Depth::NetworkReachability),
            connected,
        );
        assert_eq!(report.execution_status, ExecutionStatus::Canceled);
        assert_eq!(
            checked(&report, CheckId::NameResolution).coverage,
            Coverage::Passed
        );
        assert_eq!(
            checked(&report, CheckId::InstanceResolution).coverage,
            Coverage::NotApplicable
        );
        let tcp = checked(&report, CheckId::TcpConnect);
        assert_eq!(tcp.coverage, Coverage::Passed, "{tcp:#?}");
        assert_eq!(
            tcp.attempts.len(),
            1,
            "the abandoned connect is not a result"
        );
        assert!(report.errors.is_empty(), "{:#?}", report.errors);
        assert_eq!(
            report.deepest_phase_evaluated(),
            Some(Depth::NetworkReachability)
        );
        assert_eq!(report.exit_code(), 4);

        // Cut short while resolving: nothing failed, the rest is canceled.
        for (server, stages) in [
            (
                "tcp:db01,1433",
                Stages::new().add(Stage::Dns, None, 0, 900, &[(cs::HOST, "db01")]),
            ),
            (
                r"db01\SQL2022",
                Stages::new().add(
                    Stage::InstanceLookup,
                    None,
                    0,
                    900,
                    &[(cs::SERVER, "db01"), (cs::INSTANCE, "SQL2022")],
                ),
            ),
        ] {
            let report = canceled_with(request(server, Depth::NetworkReachability), stages);
            for check in &report.checks {
                assert!(!check.coverage.failed(), "{server}: {check:#?}");
            }
            for id in [CheckId::NameResolution, CheckId::TcpConnect] {
                assert_eq!(
                    checked(&report, id).skip,
                    Some(Skip::Canceled),
                    "{server}: {id:?}"
                );
            }
            assert!(report.errors.is_empty(), "{server}: {:#?}", report.errors);
            assert_eq!(report.exit_code(), 4);
        }
    }

    /// Phases the cancel cut short (pre-login started, not finished) do not
    /// fail the connection attempt: it is skipped as canceled.
    #[test]
    fn a_phase_a_cancel_cut_short_is_not_a_failure() {
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(true),
                1,
                2,
                &[(cs::ADDRESS, "10.0.0.1"), (cs::PORT, "1433")],
            )
            .add(Stage::Prelogin, None, 2, 900, &[]);
        let report = canceled_with(request("tcp:db01,1433", Depth::SessionValidation), stages);
        for id in [CheckId::ConnectionAttempt, CheckId::SessionValidation] {
            assert_eq!(checked(&report, id).skip, Some(Skip::Canceled), "{id:?}");
        }
        assert_eq!(
            checked(&report, CheckId::TcpConnect).coverage,
            Coverage::Passed
        );
        assert!(report.errors.is_empty(), "{:#?}", report.errors);
        assert_eq!(report.exit_code(), 4);
    }

    /// A run canceled while the client waits on the server keeps the checks
    /// the client already finished: a server that accepts TCP and never
    /// answers, canceled after a second, passes name resolution and the TCP
    /// connect, and the connection attempt is skipped as canceled, not failed.
    #[test]
    fn a_cancel_during_the_attempt_keeps_the_network_checks() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _held: Vec<_> = listener.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(10));
        });
        let started = Instant::now();
        let report = run_until(
            request(&format!("tcp:127.0.0.1,{port}"), Depth::SessionValidation),
            &|| started.elapsed() > Duration::from_secs(1),
        );
        assert_eq!(report.execution_status, ExecutionStatus::Canceled);
        assert_eq!(
            checked(&report, CheckId::NameResolution).coverage,
            Coverage::Passed,
            "{report:#?}"
        );
        assert_eq!(
            checked(&report, CheckId::TcpConnect).coverage,
            Coverage::Passed,
            "{report:#?}"
        );
        assert_eq!(
            checked(&report, CheckId::ConnectionAttempt).skip,
            Some(Skip::Canceled),
            "{report:#?}"
        );
        assert_eq!(
            report.deepest_phase_evaluated(),
            Some(Depth::NetworkReachability)
        );
        assert!(
            report.checks.iter().all(|c| !c.coverage.failed()),
            "{report:#?}"
        );
        assert_eq!(report.exit_code(), 4);
    }

    /// A run canceled while the client waits on an address leaves no error
    /// behind: the canceled report lists no address, so none is redacted.
    #[test]
    fn a_run_canceled_part_way_leaves_no_error() {
        let started = Instant::now();
        let report = run_until(
            request("tcp:192.0.2.1,1433", Depth::NetworkReachability),
            &|| started.elapsed() > Duration::from_millis(300),
        );
        assert_eq!(report.execution_status, ExecutionStatus::Canceled);
        assert!(report.errors.is_empty(), "{:#?}", report.errors);
        assert_eq!(
            checked(&report, CheckId::TcpConnect).skip,
            Some(Skip::Canceled)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A Browser request this computer would not let out is the client's own
    /// limit, so the run is partial; a send the network refused is not.
    #[test]
    fn a_browser_request_this_computer_refuses_is_a_local_failure() {
        let permission = if cfg!(windows) { 10013 } else { 13 };
        let refused = if cfg!(windows) { 10061 } else { 111 };
        assert!(ssrp_local_failure("socketFailed", None));
        assert!(ssrp_local_failure("sendFailed", Some(permission)));
        assert!(!ssrp_local_failure("sendFailed", Some(refused)));
        assert!(!ssrp_local_failure("sendFailed", None));
        assert!(!ssrp_local_failure("noAnswer", None));
    }

    /// The Browser's failure is evidence only: the client's message, which
    /// also gives advice and names the server, is not copied into the report.
    #[test]
    fn a_browser_failure_is_described_without_advice() {
        for code in [
            "sendFailed",
            "noAnswer",
            "timedOut",
            "socketFailed",
            "noAddresses",
            "noTcpPort",
            "nameResolutionFailed",
            "receiveFailed",
            "invalidResponse",
        ] {
            let problem = ssrp_problem(code, None);
            assert!(!problem.is_empty());
            for advice in ["Verify", "Ensure", "Contact", "Check "] {
                assert!(!problem.contains(advice), "{problem}");
            }
        }
    }

    /// `np:host\instance`: the client asks SQL Server Browser for the pipe;
    /// with no answer it does not connect, and no pipe is reported. Named pipes
    /// are Windows-only in the client.
    #[cfg(windows)]
    #[test]
    fn a_named_pipe_instance_is_looked_up_as_the_client_does() {
        let report = run(request(
            "np:127.0.0.1\\NoSuchDiagnosePipe",
            Depth::EndpointResolution,
        ));
        let lookup = checked(&report, CheckId::InstanceResolution);
        assert_eq!(lookup.skip, None, "{report:#?}");
        assert_eq!(lookup.field("instance"), Some("NoSuchDiagnosePipe"));
        assert_ne!(lookup.coverage, Coverage::Passed);
        assert_eq!(lookup.field("pipe"), None, "{report:#?}");
    }

    /// Diagnose checks the transports the client tries; nothing is left to
    /// a note that only TCP was checked.
    #[test]
    fn no_transport_is_left_unchecked() {
        for server in [
            "127.0.0.1,1",
            r"no-such-host.invalid\SQL1",
            r"tcp:no-such-host.invalid\SQL1",
        ] {
            let report = run(request(server, Depth::NetworkReachability));
            assert!(
                !report.limitations.iter().any(|l| l.contains("TCP only")),
                "{server}: {report:#?}"
            );
        }
        let report = run(request("127.0.0.1,1", Depth::SessionValidation));
        assert!(report.checks.iter().all(|c| c.field("transport").is_none()));
        assert!(report.errors.iter().all(|e| !e.superseded));
    }

    /// The client's connects to the addresses after the one it used run in
    /// tasks of their own; the diagnosis records them as it records the rest.
    #[test]
    fn the_connects_to_the_other_addresses_are_recorded() {
        let resolved: Vec<std::net::SocketAddr> =
            std::net::ToSocketAddrs::to_socket_addrs(&("localhost", 0))
                .unwrap()
                .collect();
        let first = std::net::TcpListener::bind((resolved[0].ip(), 0)).unwrap();
        let port = first.local_addr().unwrap().port();
        let mut listeners = vec![first];
        for address in &resolved[1..] {
            if let Ok(listener) = std::net::TcpListener::bind((address.ip(), port)) {
                listeners.push(listener);
            }
        }
        if listeners.len() < 2 {
            // This computer gives localhost one address.
            return;
        }
        let report = run(request(
            &format!("tcp:localhost,{port}"),
            Depth::NetworkReachability,
        ));
        let tcp = checked(&report, CheckId::TcpConnect);
        assert_eq!(tcp.attempts.len(), listeners.len(), "{report:#?}");
        assert!(
            tcp.attempts
                .iter()
                .all(|a| a.result == TcpResult::Connected),
            "{report:#?}"
        );
    }

    /// `tcp`, the TCP path of `db01`, then the named pipe the client falls
    /// back to, which opened.
    fn fell_back_to_a_pipe(tcp: Stages) -> Stages {
        tcp.add(
            Stage::Pipe,
            Some(true),
            50,
            52,
            &[(cs::PIPE_PATH, r"\\db01\pipe\sql\query")],
        )
    }

    /// Without a prefix, TCP did not connect and the client's named pipe
    /// then opened: the TCP path's checks are not on the connection's path.
    /// They neither fail nor set the outcome, the status or the specialist
    /// area; their errors stay as superseded evidence.
    #[test]
    fn a_tcp_path_a_working_pipe_replaced_does_not_fail() {
        let local = if cfg!(windows) { "10024" } else { "24" };
        let refused = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(false),
                1,
                5,
                &[
                    (cs::ADDRESS, "10.0.0.1"),
                    (cs::PORT, "1433"),
                    (cs::IO_KIND, "ConnectionRefused"),
                    (cs::OS_ERROR, "10061"),
                    (cs::ERROR, "refused"),
                ],
            );
        let unresolved = Stages::new().add(
            Stage::Dns,
            Some(false),
            0,
            5,
            &[
                (cs::HOST, "db01"),
                (cs::OS_ERROR, "11001"),
                (cs::ERROR, "No such host is known."),
            ],
        );
        // This computer would not open the socket: not partial either.
        let local_refusal = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(false),
                1,
                2,
                &[
                    (cs::ADDRESS, "10.0.0.1"),
                    (cs::PORT, "1433"),
                    (cs::OS_ERROR, local),
                    (cs::ERROR, "too many open sockets"),
                ],
            );
        for (case, tcp) in [
            ("refused", refused),
            ("unresolved", unresolved),
            ("local refusal", local_refusal),
        ] {
            let report = observed(
                request("db01", Depth::NetworkReachability),
                fell_back_to_a_pipe(tcp),
                Ended::Stopped,
            );
            assert_eq!(
                report.outcome,
                DiagnosticOutcome::Passed,
                "{case}: {report:#?}"
            );
            assert_eq!(
                report.execution_status,
                ExecutionStatus::Completed,
                "{case}"
            );
            assert_eq!(report.exit_code(), 0, "{case}");
            assert_eq!(report.domain, None, "{case}");
            assert!(
                report.limitations.is_empty(),
                "{case}: {:#?}",
                report.limitations
            );
            assert_eq!(
                checked(&report, CheckId::TcpConnect).skip,
                Some(Skip::NotApplicable(NOT_ON_THE_PATH)),
                "{case}"
            );
            assert!(
                report.checks.iter().all(|c| !c.coverage.failed()),
                "{case}: {:#?}",
                report.checks
            );
            assert!(
                report
                    .findings
                    .iter()
                    .all(|f| f.certainty == Certainty::Informational
                        && f.check == CheckId::ConnectionAttempt),
                "{case}: {:#?}",
                report.findings
            );
            assert!(!report.errors.is_empty(), "{case}");
            assert!(
                report.errors.iter().all(|e| e.superseded),
                "{case}: {:#?}",
                report.errors
            );
        }
    }

    /// Without a prefix, shared memory to this computer that ends the run at
    /// `networkReachability` is worded as the shared-memory provider's, the
    /// transport its check names, not as TCP: abandoned by the time limit,
    /// and refused by the operating system.
    #[test]
    fn a_shared_memory_open_that_ends_the_run_is_worded_as_shared_memory() {
        let pipe = r"\\.\pipe\SQLLocal\MSSQLSERVER";
        for (ok, details, ended, code) in [
            (None, vec![], timed_out(), 121),
            (
                Some(false),
                vec![
                    (cs::OS_ERROR, "2"),
                    (
                        cs::ERROR,
                        "The system cannot find the file specified. (os error 2)",
                    ),
                ],
                Ended::Failed(Error::Io(std::io::Error::from_raw_os_error(2))),
                2,
            ),
        ] {
            let mut fields = vec![(cs::KIND, cs::KIND_SHARED_MEMORY), (cs::PIPE_PATH, pipe)];
            fields.extend(details);
            let report = observed(
                request("localhost", Depth::NetworkReachability),
                Stages::new().add(Stage::Pipe, ok, 0, 900, &fields),
                ended,
            );
            let check = checked(&report, CheckId::TcpConnect);
            assert_eq!(check.field("transport"), Some("sharedMemory"), "{check:#?}");
            let error = report
                .errors
                .iter()
                .find(|e| !e.superseded && e.source == "os")
                .unwrap();
            assert_eq!(
                error.text.as_deref(),
                Some(
                    format!("Shared Memory Provider: Could not open a connection to SQL Server [{code}]. ")
                        .as_str()
                ),
                "{:#?}",
                report.errors
            );
            let text = report::text(&report);
            assert!(!text.contains("TCP Provider"), "{text}");
        }
    }

    /// An explicit pipe that does not open at `networkReachability` is a
    /// failed reachability check on that transport, with its error: not a
    /// clean run, and not a TCP attempt.
    #[test]
    fn a_pipe_that_does_not_open_fails_reachability() {
        let missing = Stages::new().add(
            Stage::Pipe,
            Some(false),
            0,
            4,
            &[
                (cs::PIPE_PATH, r"\\db01\pipe\sql\query"),
                (cs::OS_ERROR, "2"),
                (
                    cs::ERROR,
                    "The system cannot find the file specified. (os error 2)",
                ),
            ],
        );
        let report = observed(
            request(r"np:\\db01\pipe\sql\query", Depth::NetworkReachability),
            missing,
            Ended::Failed(Error::Io(std::io::Error::from_raw_os_error(2))),
        );
        let check = checked(&report, CheckId::TcpConnect);
        assert_eq!(check.coverage, Coverage::Diagnosed, "{report:#?}");
        assert_eq!(check.field("transport"), Some("namedPipe"));
        assert_eq!(check.field("pipe"), Some(r"\\db01\pipe\sql\query"));
        assert_eq!(check.field("port"), None);
        assert!(check.attempts.is_empty());
        assert_eq!(report.outcome, DiagnosticOutcome::IssueDetected);
        assert_eq!(report.exit_code(), 1);
        let errors: Vec<&ErrorRecord> = report
            .errors
            .iter()
            .filter(|e| !e.superseded && e.source == "os")
            .collect();
        assert_eq!(errors.len(), 1, "{:#?}", report.errors);
        assert_eq!(errors[0].check, CheckId::TcpConnect);
        assert_eq!(errors[0].os_error, Some(2));
        // Worded as the driver words a pipe that does not open, not as TCP.
        assert_eq!(
            errors[0].text.as_deref(),
            Some("Named Pipes Provider: Could not open a connection to SQL Server [2]. ")
        );
        let text = report::text(&report);
        assert!(!text.contains("TCP Provider"), "{text}");
        assert!(
            text.contains(r"namedPipe \\db01\pipe\sql\query: failed"),
            "{text}"
        );
        assert!(!text.contains("port :"), "{text}");
        assert!(
            report::json(&report, "v")
                .unwrap()
                .contains("\"transport\": \"namedPipe\""),
            "{report:#?}"
        );

        // No answer in the time limit: inconclusive. Deeper, the connection
        // attempt reports it, once.
        let unanswered = || {
            Stages::new().add(
                Stage::Pipe,
                None,
                0,
                3000,
                &[(cs::PIPE_PATH, r"\\db01\pipe\sql\query")],
            )
        };
        let report = observed(
            request(r"np:\\db01\pipe\sql\query", Depth::NetworkReachability),
            unanswered(),
            timed_out(),
        );
        let check = checked(&report, CheckId::TcpConnect);
        assert_eq!(check.coverage, Coverage::Inconclusive, "{report:#?}");
        assert_eq!(check.field("result"), Some("timedOut"));
        assert_eq!(report.exit_code(), 2);
        let report = observed(
            request(r"np:\\db01\pipe\sql\query", Depth::SessionValidation),
            unanswered(),
            timed_out(),
        );
        assert_eq!(
            checked(&report, CheckId::TcpConnect).skip,
            Some(Skip::NotApplicable(NOT_TCP))
        );
        assert!(
            checked(&report, CheckId::ConnectionAttempt)
                .coverage
                .failed(),
            "{report:#?}"
        );
        assert!(
            report
                .errors
                .iter()
                .all(|e| e.check == CheckId::ConnectionAttempt),
            "{:#?}",
            report.errors
        );
        assert_ne!(report.exit_code(), 0);

        // The pipe's own wait for a free instance timed out (all instances
        // busy, the operating system's semaphore time-out 121) before the
        // login limit: the open failed, but with no answer in time, so it is
        // inconclusive too, with the error as it was recorded.
        let busy = |path: &str, kind: &str, os_error: Option<&str>, error: &str| {
            let mut details = vec![
                (cs::PIPE_PATH, path.to_string()),
                (cs::KIND, kind.to_string()),
                (cs::IO_KIND, "TimedOut".to_string()),
                (cs::ERROR, error.to_string()),
            ];
            if let Some(code) = os_error {
                details.push((cs::OS_ERROR, code.to_string()));
            }
            let details: Vec<(&'static str, &str)> =
                details.iter().map(|(k, v)| (*k, v.as_str())).collect();
            Stages::new().add(Stage::Pipe, Some(false), 0, 5008, &details)
        };
        let semaphore = "The semaphore timeout period has expired. (os error 121)";
        let all_busy = "Named pipe connection timed out after 5001ms: all pipe instances busy";
        for (server, kind, path, os_error, error) in [
            (
                r"np:\\db01\pipe\sql\query",
                cs::KIND_NAMED_PIPE,
                r"\\db01\pipe\sql\query",
                Some("121"),
                semaphore,
            ),
            (
                r"np:\\db01\pipe\sql\query",
                cs::KIND_NAMED_PIPE,
                r"\\db01\pipe\sql\query",
                None,
                all_busy,
            ),
            (
                "lpc:localhost",
                cs::KIND_SHARED_MEMORY,
                r"\\.\pipe\SQLLocal\MSSQLSERVER",
                Some("121"),
                semaphore,
            ),
            (
                r"(localdb)\MSSQLLocalDB",
                cs::KIND_LOCALDB,
                r"\\.\pipe\LOCALDB#12345678\tsql\query",
                None,
                all_busy,
            ),
        ] {
            for depth in [Depth::NetworkReachability, Depth::SessionValidation] {
                let deeper = depth == Depth::SessionValidation;
                let mut request = request(server, depth);
                request.login_timeout_seconds = 15;
                let failed = std::io::Error::new(std::io::ErrorKind::TimedOut, error);
                let report = observed(
                    request,
                    busy(path, kind, os_error, error),
                    Ended::Failed(Error::Io(failed)),
                );
                // Deeper, the connection attempt reports it, as it does the
                // abandoned pipe.
                let id = if deeper {
                    assert_eq!(
                        checked(&report, CheckId::TcpConnect).skip,
                        Some(Skip::NotApplicable(NOT_TCP)),
                        "{server}"
                    );
                    CheckId::ConnectionAttempt
                } else {
                    CheckId::TcpConnect
                };
                let check = checked(&report, id);
                assert_eq!(
                    check.coverage,
                    Coverage::Inconclusive,
                    "{server}: {report:#?}"
                );
                let errors: Vec<&ErrorRecord> = report
                    .errors
                    .iter()
                    .filter(|e| !e.superseded && e.source == "os")
                    .collect();
                assert_eq!(errors.len(), 1, "{server}: {:#?}", report.errors);
                assert_eq!(errors[0].check, id, "{server}");
                assert_eq!(errors[0].code.as_deref(), Some("timedOut"), "{server}");
                assert_eq!(errors[0].message, error, "{server}");
                // Worded as the driver words a pipe that did not open in
                // time, as for the abandoned pipe: SNI's error 40 with
                // `WaitNamedPipe`'s 121, `Login timeout expired` and the
                // network summary.
                let provider = if kind == cs::KIND_SHARED_MEMORY {
                    "Shared Memory Provider: "
                } else {
                    "Named Pipes Provider: "
                };
                assert_eq!(
                    errors[0].text.as_deref(),
                    Some(
                        format!("{provider}Could not open a connection to SQL Server [121]. ")
                            .as_str()
                    ),
                    "{server}"
                );
                let codes: Vec<&str> = report
                    .errors
                    .iter()
                    .filter(|e| !e.superseded && e.source == "client")
                    .filter_map(|e| e.code.as_deref())
                    .collect();
                assert_eq!(
                    codes,
                    ["loginTimeoutExpired", "networkRelatedError"],
                    "{server}: {:#?}",
                    report.errors
                );
                assert!(
                    report.findings.iter().any(|f| f.text.contains(error)),
                    "{server}: {:#?}",
                    report.findings
                );
                if deeper {
                    // A connection attempt that did not succeed.
                    assert_eq!(report.outcome, DiagnosticOutcome::IssueDetected, "{server}");
                    assert_eq!(report.exit_code(), 1, "{server}");
                } else {
                    assert_eq!(check.field("result"), Some("timedOut"), "{server}");
                    assert_eq!(check.field("pipe"), Some(path), "{server}");
                    assert_eq!(
                        errors[0].os_error,
                        os_error.map(|c| c.parse().unwrap()),
                        "{server}"
                    );
                    assert_eq!(report.outcome, DiagnosticOutcome::Inconclusive, "{server}");
                    assert_eq!(report.exit_code(), 2, "{server}");
                }
            }
        }
    }

    /// `tcp:host\instance` needs a TCP port: a Browser answer with only a
    /// pipe is no usable port, and the port is asked for. Without a prefix
    /// the pipe is usable.
    #[test]
    fn an_explicit_tcp_instance_needs_a_tcp_port() {
        let pipe_only = || {
            Stages::new().add(
                Stage::InstanceLookup,
                Some(true),
                0,
                3,
                &[
                    (cs::SERVER, "db01"),
                    (cs::INSTANCE, "SQL2022"),
                    (cs::ADDRESSES, "10.0.0.1"),
                    (cs::PIPE_PATH, r"\\db01\pipe\MSSQL$SQL2022\sql\query"),
                ],
            )
        };
        let report = observed(
            request(r"tcp:db01\SQL2022", Depth::NetworkReachability),
            pipe_only(),
            Ended::Failed(Error::ConnectionError("no TCP transport".to_string())),
        );
        let lookup = checked(&report, CheckId::InstanceResolution);
        assert_eq!(lookup.coverage, Coverage::Diagnosed, "{report:#?}");
        assert_eq!(lookup.field("result"), Some("noTcpPort"));
        assert_port_requested(&report);
        assert_eq!(
            checked(&report, CheckId::TcpConnect).skip,
            Some(Skip::PortRequired)
        );
        assert_ne!(report.exit_code(), 0);

        let report = observed(
            request(r"db01\SQL2022", Depth::EndpointResolution),
            pipe_only(),
            Ended::Stopped,
        );
        assert_eq!(
            checked(&report, CheckId::InstanceResolution).coverage,
            Coverage::Passed
        );
    }

    /// The login time limit spent on SQL Server Browser before any TCP
    /// connect: TCP was needed, so the check is not attempted, with the
    /// time-limit error, not not applicable. A pipe instance keeps its skip.
    #[test]
    fn tcp_left_unreached_by_the_time_limit_is_not_attempted() {
        let answered = || {
            Stages::new().add(
                Stage::InstanceLookup,
                Some(true),
                0,
                1500,
                &[
                    (cs::SERVER, "db01"),
                    (cs::INSTANCE, "SQL2022"),
                    (cs::ADDRESSES, "10.0.0.1"),
                    (cs::PORT, "50001"),
                ],
            )
        };
        for server in [r"db01\SQL2022", r"tcp:db01\SQL2022"] {
            let mut short = request(server, Depth::SessionValidation);
            short.login_timeout_seconds = 1;
            let report = observed(short, answered(), timed_out());
            let tcp = checked(&report, CheckId::TcpConnect);
            assert_eq!(
                tcp.coverage,
                Coverage::Inconclusive,
                "{server}: {report:#?}"
            );
            assert_eq!(tcp.field("result"), Some("notAttempted"));
            assert_eq!(tcp.field("port"), Some("50001"));
            assert!(
                report
                    .errors
                    .iter()
                    .any(|e| e.check == CheckId::TcpConnect && !e.superseded),
                "{server}: {:#?}",
                report.errors
            );
            assert_eq!(report.exit_code(), 2, "{server}");
            assert_eq!(
                checked(&report, CheckId::ConnectionAttempt).skip,
                Some(Skip::BlockedBy(CheckId::TcpConnect)),
                "{server}"
            );
            let text = report::text(&report);
            assert!(text.contains("port 50001: notAttempted"), "{text}");

            let report = canceled_with(request(server, Depth::NetworkReachability), answered());
            assert_eq!(
                checked(&report, CheckId::TcpConnect).skip,
                Some(Skip::Canceled),
                "{server}"
            );
        }

        let pipe = Stages::new()
            .add(
                Stage::InstanceLookup,
                Some(true),
                0,
                3,
                &[
                    (cs::SERVER, "db01"),
                    (cs::INSTANCE, "SQL2022"),
                    (cs::PIPE_PATH, r"\\db01\pipe\MSSQL$SQL2022\sql\query"),
                ],
            )
            .add(
                Stage::Pipe,
                Some(true),
                3,
                5,
                &[(cs::PIPE_PATH, r"\\db01\pipe\MSSQL$SQL2022\sql\query")],
            );
        let report = observed(
            request(r"np:db01\SQL2022", Depth::NetworkReachability),
            pipe,
            Ended::Stopped,
        );
        assert_eq!(
            checked(&report, CheckId::TcpConnect).skip,
            Some(Skip::NotApplicable(NOT_TCP))
        );
        assert_eq!(report.exit_code(), 0);
    }

    /// A cancel received before the checks finish is never lost to one that
    /// finishes at once.
    #[test]
    fn a_cancel_already_received_wins() {
        for _ in 0..50 {
            for (server, depth) in [
                ("tcp:db01,1433", Depth::ConnectionInput),
                ("tcp:db01,", Depth::ConnectionInput),
                ("tcp:db01,", Depth::SessionValidation),
            ] {
                let report = run_until(request(server, depth), &|| true);
                assert_eq!(
                    report.execution_status,
                    ExecutionStatus::Canceled,
                    "{server} {depth:?}"
                );
            }
        }
    }

    /// A local failure (no sockets left) on the first address leaves the
    /// others untried: those say nothing, so the run is partial, not a
    /// network issue.
    #[test]
    fn a_local_failure_before_untried_addresses_is_partial() {
        let local = if cfg!(windows) { 10024 } else { 24 };
        let stages = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[
                    (cs::HOST, "db01"),
                    (cs::ADDRESSES, "10.0.0.1, 10.0.0.2, 10.0.0.3"),
                ],
            )
            .add(
                Stage::Tcp,
                Some(false),
                1,
                2,
                &[
                    (cs::ADDRESS, "10.0.0.1"),
                    (cs::PORT, "1433"),
                    (cs::OS_ERROR, &local.to_string()),
                    (cs::ERROR, "too many open sockets"),
                ],
            );
        let report = observed(
            request("tcp:db01,1433", Depth::NetworkReachability),
            stages,
            Ended::Failed(Error::Io(std::io::Error::from_raw_os_error(local))),
        );
        let tcp = checked(&report, CheckId::TcpConnect);
        let results: Vec<TcpResult> = tcp.attempts.iter().map(|a| a.result).collect();
        assert_eq!(
            results,
            [
                TcpResult::Failed,
                TcpResult::NotAttempted,
                TcpResult::NotAttempted
            ]
        );
        assert_eq!(tcp.coverage, Coverage::Inconclusive);
        assert_eq!(tcp.domain.as_ref().unwrap().primary, "client.platform");
        assert_eq!(report.execution_status, ExecutionStatus::Partial);
        assert_eq!(report.exit_code(), 3);
    }

    /// A pipe that is not there, with the real client: reachability fails
    /// with its error, and the run does not pass.
    #[cfg(windows)]
    #[test]
    fn a_missing_pipe_is_not_reachable() {
        let report = run(request(
            r"np:\\.\pipe\NoSuchDiagnosePipe\sql\query",
            Depth::NetworkReachability,
        ));
        let check = checked(&report, CheckId::TcpConnect);
        assert!(check.coverage.failed(), "{report:#?}");
        assert_eq!(check.field("transport"), Some("namedPipe"));
        assert!(report.errors.iter().any(|e| !e.superseded), "{report:#?}");
        assert_ne!(report.exit_code(), 0);
    }

    /// Without a prefix, a name that does not resolve sends the client on to
    /// the server's standard pipe; when that does not open either, the pipe's
    /// failure is the connection's, at the reachability depth too: TCP
    /// connect carries it, and the name lookup's failure stays as superseded
    /// evidence, not on the connection's path.
    #[cfg(windows)]
    #[test]
    fn a_failed_pipe_fallback_is_not_reachable() {
        let report = run(request(
            "pr729-no-such-host.invalid",
            Depth::NetworkReachability,
        ));
        let check = checked(&report, CheckId::TcpConnect);
        assert!(check.coverage.failed(), "{report:#?}");
        assert_eq!(check.field("transport"), Some("namedPipe"), "{check:#?}");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.check == CheckId::TcpConnect && !e.superseded),
            "{:#?}",
            report.errors
        );
        assert!(
            report
                .errors
                .iter()
                .filter(|e| e.check == CheckId::NameResolution)
                .all(|e| e.superseded),
            "{:#?}",
            report.errors
        );
        assert_eq!(
            checked(&report, CheckId::NameResolution).skip,
            Some(Skip::NotApplicable(NOT_ON_THE_PATH))
        );
        assert_ne!(report.outcome, DiagnosticOutcome::Passed);
        assert_ne!(report.exit_code(), 0);

        // At the default depth the connection attempt reports the pipe.
        let report = run(request(
            "pr729-no-such-host.invalid",
            Depth::SessionValidation,
        ));
        let attempt = checked(&report, CheckId::ConnectionAttempt);
        assert!(attempt.coverage.failed(), "{report:#?}");
        assert_eq!(
            attempt.field("transport"),
            Some("namedPipe"),
            "{attempt:#?}"
        );
        assert!(
            report.errors.iter().any(|e| !e.superseded),
            "{:#?}",
            report.errors
        );
        assert_ne!(report.outcome, DiagnosticOutcome::Passed);
    }

    /// The same through the stages, for a name that did not resolve and for
    /// a TCP connect that was refused: the pipe the protocol order then tried
    /// carries the failure, and what failed before it is superseded.
    #[test]
    fn a_failed_pipe_after_a_failed_tcp_path_carries_the_failure() {
        let pipe = r"\\db01\pipe\sql\query";
        let pipe_failed = (
            Stage::Pipe,
            Some(false),
            &[
                (cs::KIND, cs::KIND_NAMED_PIPE),
                (cs::PIPE_PATH, pipe),
                (cs::OS_ERROR, "53"),
                (cs::ERROR, "The network path was not found. (os error 53)"),
            ][..],
        );
        let not_found = Stages::new()
            .add(
                Stage::Dns,
                Some(false),
                0,
                5,
                &[
                    (cs::HOST, "db01"),
                    (cs::OS_ERROR, "11001"),
                    (cs::ERROR, "No such host is known."),
                ],
            )
            .add(pipe_failed.0, pipe_failed.1, 5, 9, pipe_failed.2);
        let refused = Stages::new()
            .add(
                Stage::Dns,
                Some(true),
                0,
                1,
                &[(cs::HOST, "db01"), (cs::ADDRESSES, "10.0.0.1")],
            )
            .add(
                Stage::Tcp,
                Some(false),
                1,
                3,
                &[
                    (cs::ADDRESS, "10.0.0.1"),
                    (cs::PORT, "1433"),
                    (cs::OS_ERROR, "10061"),
                    (cs::IO_KIND, "ConnectionRefused"),
                    (cs::ERROR, "refused"),
                ],
            )
            .add(pipe_failed.0, pipe_failed.1, 3, 9, pipe_failed.2);
        for (stages, earlier) in [
            (not_found, CheckId::NameResolution),
            (refused, CheckId::TcpConnect),
        ] {
            let report = observed(
                request("db01", Depth::NetworkReachability),
                stages,
                Ended::Failed(Error::Io(std::io::Error::from_raw_os_error(53))),
            );
            let tcp: Vec<&Check> = report
                .checks
                .iter()
                .filter(|c| c.id == CheckId::TcpConnect)
                .collect();
            assert_eq!(tcp.len(), 1, "{:#?}", report.checks);
            assert_eq!(tcp[0].coverage, Coverage::Diagnosed, "{:#?}", tcp[0]);
            assert_eq!(tcp[0].field("transport"), Some("namedPipe"));
            assert_eq!(tcp[0].field("pipe"), Some(pipe));
            // The summaries the driver adds follow; the event is the os one.
            let current: Vec<&ErrorRecord> = report
                .errors
                .iter()
                .filter(|e| !e.superseded && e.source == "os")
                .collect();
            assert_eq!(current.len(), 1, "{:#?}", report.errors);
            assert_eq!(current[0].check, CheckId::TcpConnect);
            assert_eq!(current[0].os_error, Some(53));
            // Worded by the transport that ended the run, not as TCP.
            assert_eq!(
                current[0].text.as_deref(),
                Some("Named Pipes Provider: Could not open a connection to SQL Server [53]. ")
            );
            assert!(
                report
                    .errors
                    .iter()
                    .any(|e| e.check == earlier && e.superseded),
                "{:#?}",
                report.errors
            );
            assert_eq!(report.outcome, DiagnosticOutcome::IssueDetected);
            assert_eq!(report.exit_code(), 1);
        }
    }

    /// `np:host\instance` is diagnosed on its pipe: the text names the pipe
    /// the Browser listed, not the TCP port it also listed.
    #[test]
    fn a_pipe_instance_reads_as_its_pipe() {
        let pipe = r"\\db01\pipe\MSSQL$SQL2022\sql\query";
        let stages = || {
            Stages::new()
                .add(
                    Stage::InstanceLookup,
                    Some(true),
                    0,
                    3,
                    &[
                        (cs::SERVER, "db01"),
                        (cs::INSTANCE, "SQL2022"),
                        (cs::ADDRESSES, "10.0.0.1"),
                        (cs::PORT, "50001"),
                        (cs::PIPE_PATH, pipe),
                    ],
                )
                .add(Stage::Pipe, Some(true), 3, 5, &[(cs::PIPE_PATH, pipe)])
        };
        let report = observed(
            request(r"np:db01\SQL2022", Depth::NetworkReachability),
            stages(),
            Ended::Stopped,
        );
        let text = report::text(&report);
        assert!(
            text.contains(&format!(r"db01\SQL2022: pipe {pipe}")),
            "{text}"
        );
        assert!(!text.contains("port 50001"), "{text}");

        let report = observed(
            request(r"db01\SQL2022", Depth::EndpointResolution),
            stages(),
            Ended::Stopped,
        );
        let text = report::text(&report);
        assert!(text.contains(r"db01\SQL2022: port 50001"), "{text}");
    }

    /// A login refused with no ticket cache claims no missing ticket beyond
    /// the cache looked for: without `KRB5CCNAME`, only the default file
    /// cache, as the limitation says.
    #[test]
    fn a_missing_ticket_cache_is_claimed_only_where_it_was_looked_for() {
        let (state, location) = ticket_cache_state(None, 1000, |_| false);
        assert_eq!((state, location.as_str()), ("notFound", "default"));
        let default = no_ticket_cache_finding(Some(&location));
        assert!(
            default.starts_with("The default file Kerberos ticket cache"),
            "{default}"
        );
        assert!(
            default.contains("unless krb5.conf selects another cache"),
            "{default}"
        );
        let (state, location) = ticket_cache_state(Some("FILE:/tmp/cc"), 1000, |_| false);
        assert_eq!((state, location.as_str()), ("notFound", "KRB5CCNAME"));
        assert_eq!(
            no_ticket_cache_finding(Some(&location)),
            "The Kerberos ticket cache KRB5CCNAME names was not found, so the integrated login \
             had no ticket to present."
        );
    }

    /// The session query's error quotes the client's or the server's text:
    /// its finding is flagged for review before sharing. The time limit's
    /// finding is sqlcmd's own.
    #[test]
    fn a_failed_session_query_is_flagged_for_review() {
        let failed = |error: Option<Error>| {
            let mut diagnosis = Diagnosis::new(request("tcp:db01,1433", Depth::SessionValidation));
            diagnosis.connection_input().unwrap();
            diagnosis.session_failed(
                Check::new(CheckId::SessionValidation, Coverage::Passed),
                error,
            );
            diagnosis.finish(0, 0)
        };
        let review = |report: &Report| {
            let value: serde_json::Value =
                serde_json::from_str(&report::json(report, "v").unwrap()).unwrap();
            value["reviewBeforeSharing"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == "findings[].text")
        };
        let report = failed(Some(Error::ProtocolError(
            "the server on db01 closed the session".to_string(),
        )));
        let finding = report
            .findings
            .iter()
            .find(|f| f.check == CheckId::SessionValidation)
            .unwrap();
        assert!(finding.quotes_message, "{finding:#?}");
        assert!(review(&report));

        let report = failed(None);
        let finding = report
            .findings
            .iter()
            .find(|f| f.check == CheckId::SessionValidation)
            .unwrap();
        assert!(!finding.quotes_message, "{finding:#?}");
        assert!(!review(&report));
    }
}

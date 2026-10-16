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
//! Session validation, the deepest, is the default. This build runs the
//! checks through network reachability: the client stops once its transport
//! is open, and the connection attempt and session validation are reported
//! as skipped, not checked by this build. Checks run best effort: a
//! failed check skips only the checks that need its result, each skipped check
//! names what blocked it, and every resolved address is tried. Each check
//! reports a coverage state (`passed`, `diagnosed`, `classified`,
//! `inconclusive`, `skipped`, `notApplicable`), its duration on a monotonic
//! clock, its time limit, and what it found. The [`Report`] adds the findings (confirmed,
//! suspected, informational), the ordered error chain with its stable
//! identifiers, the specialist domain for a handoff, the execution status, the
//! diagnostic outcome and the exit code. [`report`] renders it as text or JSON,
//! share-safe by default ([`redact`]).
//!
//! sqlcmd makes no network call of its own. `mssql-tds` makes one connection
//! attempt, as a normal run does (its name resolution, SQL Server Browser
//! lookup, protocol order, TCP connects or pipe, TLS and login), through its
//! diagnostic entry point, which stops where the depth asks and also connects
//! to the addresses after the one it uses. Its connect-stage spans
//! ([`stages`]) are the evidence: the checks are read from them and from how
//! the attempt ended. The password is used to log in and is never part of the
//! report.

pub mod redact;
pub mod report;
pub mod stages;
pub mod target;

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::connect_stage;
use mssql_tds::connection_provider::tds_connection_provider::{
    DiagnosticConnection, DiagnosticProbe, DiagnosticStop, TdsConnectionProvider,
};
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
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
    /// Show identifiers as they are, instead of share-safe labels.
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
    PortRequired,
    /// Why the check does not apply.
    NotApplicable(&'static str),
    /// The diagnosis was canceled before the check finished.
    Canceled,
    /// This build does not run the check yet.
    NotInThisBuild,
}

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
    pub domain: Option<Domain>,
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
    let start_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let start = Instant::now();
    let mut diagnosis = Diagnosis::new(request);

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
                    let stopped = tokio::select! {
                        () = diagnosis.execute() => false,
                        () = wait_until(canceled) => true,
                    };
                    if stopped {
                        diagnosis.canceled();
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
    recorder: StageRecorder,
    target: Option<Target>,
    checks: Vec<Check>,
    findings: Vec<Finding>,
    errors: Vec<ErrorRecord>,
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
        && matches!(
            lookup.detail(connect_stage::RESULT),
            Some("nameResolutionFailed" | "noAddresses")
        )
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
            recorder: StageRecorder::new(),
            target: None,
            checks: Vec::new(),
            findings: Vec::new(),
            errors: Vec::new(),
            limitations: Vec::new(),
            status: ExecutionStatus::Completed,
        }
    }

    /// The run was canceled: the stages the client finished before the
    /// cancel still give their checks, and every check at the requested
    /// depth that did not finish is skipped as canceled.
    fn canceled(&mut self) {
        let steps = self.recorder.steps();
        self.canceled_after(steps);
    }

    /// [Diagnosis::canceled], with the stages the client recorded before
    /// the cancel.
    fn canceled_after(&mut self, steps: Vec<Step>) {
        self.status = ExecutionStatus::Canceled;
        self.observe_so_far(steps);
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
    fn observe_so_far(&mut self, steps: Vec<Step>) {
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
        self.observe(&target, steps, &Ended::Canceled, 0);
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
            // This build does not log in: the attempt stops once its
            // transport is open.
            Depth::NetworkReachability | Depth::ConnectionAttempt | Depth::SessionValidation => {
                Some(DiagnosticStop::Transport)
            }
        };
        let probe = DiagnosticProbe {
            stop_after,
            all_addresses: true,
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
            Box::pin(TdsConnectionProvider::new().diagnose_client(context, &server, probe)),
        )
        .await;
        let attempt_ms = millis(started);
        let (client, ended) = match attempt {
            Ok(DiagnosticConnection::Connected(client)) => (Some(client), Ended::Connected),
            Ok(DiagnosticConnection::Stopped(_)) => (None, Ended::Stopped),
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
        self.observe(&target, steps, &ended, attempt_ms);
        // With a stop the client does not log in; a connection is closed.
        if let Some(mut client) = client {
            let _ = client.close_connection().await;
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
    /// and how it `ended`. `attempt_ms` is how long it took.
    fn observe(&mut self, target: &Target, mut steps: Vec<Step>, ended: &Ended, attempt_ms: u64) {
        steps.sort_by_key(|s| s.started);
        let timed_out = ended.timed_out();
        let all: Vec<&Step> = steps.iter().collect();
        let groups = transports(&all);
        let lookup = all
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
            None if lookup.is_none() => expected_pipe(target),
            None => None,
        };
        let not_tcp = (tcp.is_none() && lookup.is_none()).then(|| {
            if first_pipe == Some(Protocol::SharedMemory) && target.protocol == Protocol::Default {
                SHARED_MEMORY_FIRST
            } else {
                NOT_TCP
            }
        });

        self.moved_past(&groups, primary_at);
        let name_failed =
            self.name_resolution(target, tcp, lookup, not_tcp, timed_out, tcp_superseded);
        let pipe_used = primary.and_then(|g| g.pipe).filter(|p| p.ok == Some(true));
        let lookup_blocks =
            self.instance_resolution(target, lookup, tcp.is_some(), not_tcp, pipe_used, timed_out);
        let tcp_blocks = if self.wants(Depth::NetworkReachability) {
            let blocked = lookup_blocks
                .clone()
                .or_else(|| name_failed.then_some(Skip::BlockedBy(CheckId::NameResolution)));
            self.tcp_connect(
                target,
                tcp,
                blocked,
                not_tcp,
                tcp_superseded,
                ended.canceled(),
            )
        } else {
            None
        };

        if !self.wants(Depth::ConnectionAttempt) {
            return;
        }
        // The attempt never got a transport to the target: the check that
        // failed blocks the deeper checks. Otherwise this build does not run
        // them.
        let blocked = match primary {
            Some(group) if group.is_tcp() && !group.opened() => {
                Some(if group.dns.is_some_and(|d| d.ok != Some(true)) {
                    Skip::BlockedBy(CheckId::NameResolution)
                } else {
                    tcp_blocks.unwrap_or(Skip::BlockedBy(CheckId::TcpConnect))
                })
            }
            None => lookup_blocks,
            _ => None,
        };
        self.skip_rest(
            CheckId::TcpConnect,
            &blocked.unwrap_or(Skip::NotInThisBuild),
        );
    }

    /// The attempt failed before any connect stage (resolving a LocalDB
    /// instance, say): the failure is the deepest check's.
    fn failed_before_any_stage(&mut self, target: &Target, error: &Error, attempt_ms: u64) {
        let network = [
            CheckId::NameResolution,
            CheckId::InstanceResolution,
            CheckId::TcpConnect,
        ];
        let carrier = network
            .iter()
            .rev()
            .copied()
            .find(|id| self.wants(id.depth()))
            .unwrap_or(CheckId::NameResolution);
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
        let classified = classify_connection_error(error);
        let mut check = Check::new(carrier, classified.coverage);
        check.duration_ms = Some(attempt_ms);
        check.deadline_ms = Some(self.request.timeout_ms());
        check.domain = Some(classified.domain);
        self.checks.push(check);
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
        if !addresses.is_empty() {
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
        check.deadline_ms = Some(self.request.timeout_ms());
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
        if from_text {
            self.limitation(
                "This platform does not expose the resolver's result code; the name-resolution \
                 result was classified from the resolver's message.",
            );
        }
        true
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
        if step.ok == Some(true) {
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
        // What is blocked without the endpoint: a port, or a pipe.
        let blocks = if target.protocol.uses_tcp() {
            Skip::PortRequired
        } else {
            Skip::BlockedBy(id)
        };
        let code = match step.detail(connect_stage::RESULT) {
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
        if target.protocol.uses_tcp() {
            self.finding(
                Certainty::Informational,
                id,
                "The checks that need a port were not run: sqlcmd does not try port 1433 or any \
                 other port for a named instance whose port SQL Server Browser did not give.",
            );
        }
        self.errors.push(record);
        Some(blocks)
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
        if !check.attempts.is_empty()
            && check
                .attempts
                .iter()
                .all(|a| a.os_error.is_some_and(local_facility_error))
        {
            // This computer refused to open the connection: the check says
            // nothing about the target, so the diagnosis is partial.
            let codes: Vec<String> = check
                .attempts
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

    /// The transports the client's protocol order moved past (shared memory
    /// that did not open, a TCP path that failed): an informational finding
    /// each, and their errors kept as evidence, marked superseded, as a
    /// normal run reports only the last transport's.
    fn moved_past(&mut self, groups: &[Transport], current: Option<usize>) {
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
                None => self.finding(
                    Certainty::Informational,
                    CheckId::ConnectionAttempt,
                    "TCP did not connect; without a protocol prefix sqlcmd then tries the \
                     server's standard pipe.",
                ),
            }
        }
    }

    fn connection_input(&mut self) -> Option<Target> {
        let started = Instant::now();
        let parsed = target::parse(&self.request.server);
        let mut check = Check::new(CheckId::ConnectionInput, Coverage::Passed);
        check.duration_ms = Some(millis(started));
        match parsed {
            Ok(target) => {
                check
                    .fields
                    .push(Field::plain("protocol", target.protocol.name()));
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
                self.finding(
                    Certainty::Confirmed,
                    CheckId::ConnectionInput,
                    format!("The server given to -S could not be parsed: {error}."),
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
        context.application_name = "sqlcmd diagnose".to_string();
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
        Report {
            request: self.request,
            start_unix_ms,
            duration_ms,
            target: self.target,
            checks: self.checks,
            findings: self.findings,
            errors: self.errors,
            domain,
            execution_status: self.status,
            outcome,
            limitations: self.limitations,
        }
    }
}

/// The diagnostic outcome from the checks, in the specification's precedence.
fn aggregate(checks: &[Check]) -> DiagnosticOutcome {
    let any = |wanted: &[Coverage]| checks.iter().any(|c| wanted.contains(&c.coverage));
    if any(&[Coverage::Diagnosed, Coverage::Classified]) {
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

/// Errors from the client, as error-chain records.
fn error_records(error: &Error, check: CheckId) -> Vec<ErrorRecord> {
    match error {
        Error::Io(io) => {
            let mut record = ErrorRecord::new("os", check, io.to_string());
            record.os_error = io.raw_os_error();
            record.code = Some(TcpResult::from_io(io).name().to_string());
            vec![record]
        }
        _ => vec![ErrorRecord::new("client", check, error.to_string())],
    }
}

/// A connection-attempt failure, classified.
struct Classified {
    coverage: Coverage,
    domain: Domain,
    finding: String,
    errors: Vec<ErrorRecord>,
}

/// A failure before any connect stage: nothing more specific than the
/// client's error says where it is.
fn classify_connection_error(error: &Error) -> Classified {
    let errors = error_records(error, CheckId::ConnectionAttempt);
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
        finding: format!("The connection attempt failed: {error}"),
        errors,
    }
}

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
fn expected_pipe(target: &Target) -> Option<Protocol> {
    match target.protocol {
        Protocol::Default if cfg!(windows) && target.is_local() && target.port.is_none() => {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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
    fn connection_input_reports_the_database_share_safe() {
        let mut with_database = request("tcp:db01,1433", Depth::ConnectionInput);
        with_database.database = Some("sales".to_string());
        let report = run(with_database);
        let text = report::text(&report);
        assert!(
            text.contains("  Connection input      PASSED           <1 ms  tcp, host host-1, port 1433, database database-1\n"),
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
    /// names one, in any case (`\\.\PIPE\sql\query`): the pipe opens.
    #[cfg(windows)]
    #[test]
    fn a_live_named_pipe_passes_whatever_its_case() {
        let Ok(pipe) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_PIPE") else {
            return;
        };
        let report = run(request(&pipe, Depth::NetworkReachability));
        assert_eq!(
            report.outcome,
            DiagnosticOutcome::Passed,
            "{pipe}: {report:#?}"
        );
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

    #[test]
    fn a_live_ipv6_server_passes_with_and_without_a_prefix() {
        let Ok(server) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_IPV6") else {
            return;
        };
        for target in [server.clone(), format!("tcp:{server}")] {
            let report = run(request(&target, Depth::NetworkReachability));
            assert_eq!(
                report.outcome,
                DiagnosticOutcome::Passed,
                "{target}: {report:#?}"
            );
        }
    }
    /// A live server, when `MSSQL_SQLCMD_DIAGNOSTICS_SERVER` names one: every
    /// check passes, through network reachability, and this build does not
    /// run the deeper ones.
    #[test]
    fn a_live_server_passes_every_check() {
        let Ok(server) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_SERVER") else {
            return;
        };
        let mut live = request(&server, Depth::NetworkReachability);
        live.login_timeout_seconds = 15;
        let report = run(live);
        assert_eq!(report.outcome, DiagnosticOutcome::Passed, "{report:#?}");
        for id in [CheckId::NameResolution, CheckId::TcpConnect] {
            let check = report.checks.iter().find(|c| c.id == id).unwrap();
            // An address literal needs no lookup, so has no time limit.
            let expected = (check.field("result") != Some("addressLiteral")).then_some(15_000);
            assert_eq!(check.deadline_ms, expected, "{id:?}");
        }

        let report = run(request(&server, Depth::SessionValidation));
        assert_eq!(report.outcome, DiagnosticOutcome::NotEvaluated);
        for id in [CheckId::ConnectionAttempt, CheckId::SessionValidation] {
            assert_eq!(
                checked(&report, id).skip,
                Some(Skip::NotInThisBuild),
                "{id:?}"
            );
        }
        assert_eq!(report.exit_code(), 2, "{report:#?}");
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
    /// one: its pipe opens as the client opens it, the TCP checks not
    /// applying.
    #[cfg(windows)]
    #[test]
    fn a_live_localdb_instance_passes() {
        let Ok(instance) = std::env::var("MSSQL_SQLCMD_DIAGNOSTICS_LOCALDB") else {
            return;
        };
        let mut localdb = request(
            &format!("(localdb)\\{instance}"),
            Depth::NetworkReachability,
        );
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
    /// advice (FR-012).
    #[test]
    fn a_named_instance_without_a_port_is_reported_without_advice() {
        let report = run(request(
            "127.0.0.1\\NoSuchDiagnoseInstance",
            Depth::NetworkReachability,
        ));
        let check = |id: CheckId| report.checks.iter().find(|c| c.id == id).unwrap();
        assert_ne!(
            check(CheckId::InstanceResolution).coverage,
            Coverage::Passed
        );
        assert_eq!(check(CheckId::TcpConnect).skip, Some(Skip::PortRequired));
        assert!(!report.findings.is_empty());
        for finding in &report.findings {
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
    /// as [`Diagnosis::execute`] builds it, without a connection.
    fn observed(request: Request, stages: Stages, ended: Ended) -> Report {
        let mut diagnosis = Diagnosis::new(request);
        let target = diagnosis.connection_input().unwrap();
        diagnosis.observe(&target, stages.list, &ended, 10);
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
    }

    /// A stage the client abandoned (a time limit or a cancel dropped it) is
    /// not a failure: a name lookup it cut short is inconclusive, not
    /// diagnosed.
    #[test]
    fn an_abandoned_stage_is_not_a_failure() {
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

    /// This build stops once the transport is open: past it, every check at
    /// the depth is reported as not checked by this build.
    #[test]
    fn the_checks_past_the_transport_are_not_run_by_this_build() {
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
            );
        let report = observed(
            request("tcp:db01,1433", Depth::SessionValidation),
            stages,
            Ended::Stopped,
        );
        assert_eq!(
            checked(&report, CheckId::TcpConnect).coverage,
            Coverage::Passed
        );
        for id in [CheckId::ConnectionAttempt, CheckId::SessionValidation] {
            let check = checked(&report, id);
            assert_eq!(check.coverage, Coverage::Skipped, "{id:?}");
            assert_eq!(check.skip, Some(Skip::NotInThisBuild), "{id:?}");
        }
        assert_eq!(report.outcome, DiagnosticOutcome::NotEvaluated);
        assert_eq!(
            report.deepest_phase_evaluated(),
            Some(Depth::NetworkReachability)
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

    /// The report of a run canceled after the client recorded `stages`.
    fn canceled_with(request: Request, stages: Stages) -> Report {
        let mut diagnosis = Diagnosis::new(request);
        diagnosis.connection_input().unwrap();
        diagnosis.canceled_after(stages.list);
        diagnosis.finish(0, 0)
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
}

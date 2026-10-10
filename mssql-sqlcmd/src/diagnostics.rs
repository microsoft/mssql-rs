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
//! | Endpoint resolution | resolve the host name; ask SQL Server Browser for a named instance's port |
//! | Network reachability | a TCP connect to every resolved address |
//! | Connection attempt | one normal connection attempt, with its pre-login, TLS and login phases |
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
//! share-safe by default ([`redact`]).
//!
//! sqlcmd resolves the name, asks SQL Server Browser and connects over TCP
//! itself. The connection attempt is made with `mssql-tds`, whose connect-stage
//! spans ([`stages`]) give the pre-login, TLS and login phases and their
//! timings. The password is used to log in and is never part of the report.

pub mod redact;
pub mod report;
pub mod stages;
pub mod target;

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mssql_tds::connection::client_context::{
    ClientContext, TdsAuthenticationMethod, TransportContext,
};
use mssql_tds::connection::tds_client::{ResultSet, StatementResult, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting, Version};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::error::Error;
use mssql_tds::ssrp::SsrpLookupError;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

use redact::Category;
use stages::{ConnectStagesOnly, Stage, StageRecorder, Step};
use target::{Protocol, Target};

/// How to log in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authentication {
    /// SQL Server authentication (`-U`/`-P`).
    SqlPassword { user: String, password: String },
    /// Windows authentication, or Kerberos off Windows (`-E`).
    Integrated,
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
    PortRequired,
    /// Why the check does not apply.
    NotApplicable(&'static str),
    /// The diagnosis was canceled before the check finished.
    Canceled,
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
        use std::io::ErrorKind;
        match error.kind() {
            ErrorKind::ConnectionRefused => TcpResult::Refused,
            ErrorKind::TimedOut => TcpResult::TimedOut,
            ErrorKind::HostUnreachable => TcpResult::HostUnreachable,
            ErrorKind::NetworkUnreachable => TcpResult::NetworkUnreachable,
            ErrorKind::ConnectionReset => TcpResult::Reset,
            _ => error
                .raw_os_error()
                .map_or(TcpResult::Failed, tcp_result_from_code),
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
    pub address: IpAddr,
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
}

impl Domain {
    fn specific(primary: &'static str) -> Self {
        Self {
            primary,
            candidates: Vec::new(),
        }
    }

    fn undetermined(candidates: &[&'static str]) -> Self {
        Self {
            primary: "undetermined",
            candidates: candidates.to_vec(),
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
    pub packet_size: u32,
    /// Whether everything after login stays encrypted.
    pub encrypted: bool,
    /// From session validation: `SERVERPROPERTY('EngineEdition')`.
    pub engine_edition: Option<i32>,
}

impl ServerInfo {
    /// The kind of target, from the engine edition.
    pub fn target_type(&self) -> Option<&'static str> {
        self.engine_edition.map(|edition| match edition {
            5 => "azureSqlDatabase",
            8 => "azureSqlManagedInstance",
            6 | 11 => "azureSynapseAnalytics",
            9 => "azureSqlEdge",
            _ => "sqlServer",
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
                Ok(runtime) => runtime.block_on(async {
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
    authentication: Option<AuthOutcome>,
    server: Option<ServerInfo>,
    /// The TCP port the connection uses, once known.
    port: Option<u16>,
    /// The pipe an `np:host\instance` connection uses, once known.
    pipe: Option<String>,
    /// The SPN the attempt requests with integrated authentication.
    spn: Option<String>,
    limitations: Vec<String>,
    status: ExecutionStatus,
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
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
            authentication: None,
            server: None,
            port: None,
            pipe: None,
            spn: None,
            limitations: Vec::new(),
            status: ExecutionStatus::Completed,
        }
    }

    /// The run was canceled: every check at the requested depth that did not
    /// finish is skipped as canceled.
    fn canceled(&mut self) {
        self.status = ExecutionStatus::Canceled;
        for id in CheckId::ORDER {
            if self.wants(id.depth()) && !self.checks.iter().any(|c| c.id == id) {
                self.checks.push(Check::skipped(id, Skip::Canceled));
            }
        }
    }
    fn wants(&self, depth: Depth) -> bool {
        self.request.depth >= depth
    }

    fn finding(&mut self, certainty: Certainty, check: CheckId, text: impl Into<String>) {
        self.findings.push(Finding {
            certainty,
            check,
            text: text.into(),
        });
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

    async fn execute(&mut self) {
        let Some(target) = self.connection_input() else {
            return;
        };
        if !self.wants(Depth::EndpointResolution) {
            return;
        }

        let over_tcp = target.protocol.uses_tcp();
        let mut port = target.port;
        let mut addresses = None;
        if over_tcp {
            addresses = self.name_resolution(&target).await;
            if target.protocol == Protocol::Admin && target.instance.is_some() {
                // A named instance's DAC port comes from SQL Server Browser's
                // DAC request, which neither this diagnosis nor the client
                // makes; neither 1434 nor the instance's port is it.
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
                     SQL Server Browser's DAC request, which this diagnosis does not make, so its \
                     endpoint and connection are not checked.",
                );
                port = None;
            } else if target.protocol == Protocol::Admin {
                // The client connects the dedicated administrator connection
                // over TCP to port 1434, whatever port or instance is given.
                self.checks.push(Check::skipped(
                    CheckId::InstanceResolution,
                    Skip::NotApplicable("the dedicated administrator connection uses port 1434"),
                ));
                port = Some(DAC_PORT);
            } else if target.needs_instance_lookup() {
                port = self.instance_resolution(&target).await;
            } else {
                self.checks.push(Check::skipped(
                    CheckId::InstanceResolution,
                    Skip::NotApplicable(if target.instance.is_some() {
                        "a port was given"
                    } else {
                        "not a named instance"
                    }),
                ));
                port = Some(port.unwrap_or(1433));
            }
        } else {
            let why = "the connection does not use TCP";
            let pipe_lookup = target.protocol == Protocol::NamedPipe && target.instance.is_some();
            for id in [
                CheckId::NameResolution,
                CheckId::InstanceResolution,
                CheckId::TcpConnect,
            ] {
                if pipe_lookup && id == CheckId::InstanceResolution {
                    self.pipe = Some(self.pipe_resolution(&target).await);
                } else if self.wants(id.depth()) {
                    self.checks
                        .push(Check::skipped(id, Skip::NotApplicable(why)));
                }
            }
        }

        self.port = if over_tcp { port } else { None };
        if !self.wants(Depth::NetworkReachability) {
            return;
        }
        if over_tcp {
            let reachable = match (addresses, port) {
                (Some(addresses), Some(port)) => self.tcp_connect(&addresses, port).await,
                (None, _) => {
                    self.skip_rest(
                        CheckId::InstanceResolution,
                        &Skip::BlockedBy(CheckId::NameResolution),
                    );
                    false
                }
                (_, None) if target.protocol == Protocol::Admin => {
                    self.skip_rest(
                        CheckId::InstanceResolution,
                        &Skip::BlockedBy(CheckId::InstanceResolution),
                    );
                    false
                }
                (_, None) => {
                    self.skip_rest(CheckId::InstanceResolution, &Skip::PortRequired);
                    false
                }
            };
            if !reachable {
                if cfg!(windows) && target.protocol == Protocol::Default {
                    self.limitation(
                        "Without a protocol prefix, sqlcmd on Windows can also connect over \
                         shared memory (to this computer) or named pipes; this diagnosis checks \
                         TCP only. Give lpc: or np: to diagnose those.",
                    );
                }
                self.skip_rest(CheckId::TcpConnect, &Skip::BlockedBy(CheckId::TcpConnect));
                return;
            }
        }

        if !self.wants(Depth::ConnectionAttempt) {
            return;
        }
        let Some(mut client) = self.connection_attempt().await else {
            self.skip_rest(
                CheckId::ConnectionAttempt,
                &Skip::BlockedBy(CheckId::ConnectionAttempt),
            );
            return;
        };
        if self.wants(Depth::SessionValidation) {
            self.session_validation(&mut client).await;
        }
        let _ = client.close_connection().await;
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

    async fn name_resolution(&mut self, target: &Target) -> Option<Vec<IpAddr>> {
        let host = target.resolvable_host().to_string();
        let mut check = Check::new(CheckId::NameResolution, Coverage::Passed);
        check
            .fields
            .push(Field::identifier("host", host.clone(), Category::Host));

        if let Ok(address) = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            check.duration_ms = Some(0);
            check.fields.push(Field::plain("result", "addressLiteral"));
            check.fields.push(Field::identifier(
                "addresses",
                address.to_string(),
                Category::Address,
            ));
            self.checks.push(check);
            return Some(vec![address]);
        }

        check.deadline_ms = Some(self.request.timeout_ms());
        let started = Instant::now();
        let resolved = tokio::time::timeout(
            self.request.timeout(),
            tokio::net::lookup_host((host.as_str(), 0)),
        )
        .await;
        check.duration_ms = Some(millis(started));

        let (code, os_error, message, from_text) = match resolved {
            Ok(Ok(found)) => {
                let mut addresses: Vec<IpAddr> = Vec::new();
                for address in found.map(|a: SocketAddr| a.ip()) {
                    if !addresses.contains(&address) {
                        addresses.push(address);
                    }
                }
                if !addresses.is_empty() {
                    check.fields.push(Field::plain("result", "resolved"));
                    for address in &addresses {
                        check.fields.push(Field::identifier(
                            "addresses",
                            address.to_string(),
                            Category::Address,
                        ));
                    }
                    self.checks.push(check);
                    return Some(addresses);
                }
                (
                    "noAddresses",
                    None,
                    "the name resolved to no address".to_string(),
                    false,
                )
            }
            Ok(Err(error)) => {
                let (code, from_text) = dns_result(&error);
                (code, error.raw_os_error(), error.to_string(), from_text)
            }
            Err(_) => (
                "timedOut",
                None,
                format!("no answer within {} s", self.request.timeout().as_secs()),
                false,
            ),
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
        self.finding(
            Certainty::Confirmed,
            CheckId::NameResolution,
            format!("Name resolution for {host} failed ({code}): {message}."),
        );
        let mut record = ErrorRecord::new("os", CheckId::NameResolution, message);
        record.os_error = os_error;
        record.code = Some(code.to_string());
        record.from_message_text = from_text;
        self.errors.push(record);
        if from_text {
            self.limitation(
                "This platform does not expose the resolver's result code; the name-resolution \
                 result was classified from the resolver's message.",
            );
        }
        None
    }

    async fn instance_resolution(&mut self, target: &Target) -> Option<u16> {
        let host = target.resolvable_host().to_string();
        let instance = target.instance.clone().unwrap_or_default();
        let timeout_ms = BROWSER_TIMEOUT_MS;
        let mut check = Check::new(CheckId::InstanceResolution, Coverage::Passed);
        check.deadline_ms = Some(timeout_ms);
        check
            .fields
            .push(Field::identifier("host", host.clone(), Category::Host));
        check.fields.push(Field::identifier(
            "instance",
            instance.clone(),
            Category::Instance,
        ));

        let started = Instant::now();
        let answer = mssql_tds::ssrp::lookup_instance(&host, &instance, timeout_ms).await;
        check.duration_ms = Some(millis(started));

        let mut record = ErrorRecord::new("sqlBrowser", CheckId::InstanceResolution, String::new());
        let problem = match answer {
            Ok(endpoints) => {
                if let Some(port) = endpoints.iter().find_map(|e| e.tcp_port) {
                    check.fields.push(Field::plain("port", port.to_string()));
                    self.checks.push(check);
                    return Some(port);
                }
                check.coverage = Coverage::Diagnosed;
                record.code = Some("noTcpPort".to_string());
                "SQL Server Browser answered but listed no TCP port for the instance".to_string()
            }
            Err(error) => {
                // An answer that could not be parsed is evidence about the
                // Browser; no answer at all is not.
                check.coverage = match error {
                    SsrpLookupError::InvalidResponse(_) => Coverage::Diagnosed,
                    _ => Coverage::Inconclusive,
                };
                record.code = Some(error.code().to_string());
                record.os_error = error.os_error();
                check.fields.push(Field::plain("result", error.code()));
                ssrp_problem(&error)
            }
        };
        check.domain = Some(if check.coverage == Coverage::Diagnosed {
            Domain::specific("connectivity.instanceResolution")
        } else {
            Domain::undetermined(&["connectivity.instanceResolution", "connectivity.network"])
        });
        self.checks.push(check);
        self.finding(
            Certainty::Confirmed,
            CheckId::InstanceResolution,
            format!(
                "SQL Server Browser on {host} gave no usable port for instance {instance}: \
                 {problem}."
            ),
        );
        self.finding(
            Certainty::Informational,
            CheckId::InstanceResolution,
            "The checks that need a port were not run: sqlcmd does not try port 1433 or any \
             other port for a named instance whose port SQL Server Browser did not give.",
        );
        record.message = problem;
        self.errors.push(record);
        None
    }

    /// `np:host\instance`: the client asks SQL Server Browser for the
    /// instance's pipe and, without an answer, uses the instance's standard
    /// pipe; so does this. A Browser that does not answer does not block.
    async fn pipe_resolution(&mut self, target: &Target) -> String {
        let host = target.resolvable_host().to_string();
        let instance = target.instance.clone().unwrap_or_default();
        let timeout_ms = BROWSER_TIMEOUT_MS;
        let mut check = Check::new(CheckId::InstanceResolution, Coverage::Passed);
        check.deadline_ms = Some(timeout_ms);
        check
            .fields
            .push(Field::identifier("host", host.clone(), Category::Host));
        check.fields.push(Field::identifier(
            "instance",
            instance.clone(),
            Category::Instance,
        ));
        let started = Instant::now();
        let answer = mssql_tds::ssrp::lookup_instance(&host, &instance, timeout_ms).await;
        check.duration_ms = Some(millis(started));
        let listed = match answer {
            Ok(endpoints) => endpoints.into_iter().find_map(|e| e.pipe_path),
            Err(error) => {
                check.coverage = Coverage::Inconclusive;
                check.domain = Some(Domain::undetermined(&[
                    "connectivity.instanceResolution",
                    "connectivity.network",
                ]));
                check.fields.push(Field::plain("result", error.code()));
                let problem = ssrp_problem(&error);
                self.finding(
                    Certainty::Informational,
                    CheckId::InstanceResolution,
                    format!(
                        "SQL Server Browser on {host} gave no pipe for instance {instance}: \
                         {problem}; the client then uses the instance's standard pipe."
                    ),
                );
                let mut record =
                    ErrorRecord::new("sqlBrowser", CheckId::InstanceResolution, problem);
                record.code = Some(error.code().to_string());
                record.os_error = error.os_error();
                self.errors.push(record);
                None
            }
        };
        let source = if listed.is_some() {
            "sqlBrowser"
        } else {
            "standard"
        };
        let pipe = local_pipe(listed.unwrap_or_else(|| standard_pipe(&target.host, &instance)));
        check
            .fields
            .push(Field::identifier("pipe", pipe.clone(), Category::Pipe));
        check.fields.push(Field::plain("pipeSource", source));
        self.checks.push(check);
        pipe
    }

    /// Connects to every address; true when at least one accepted.
    async fn tcp_connect(&mut self, addresses: &[IpAddr], port: u16) -> bool {
        self.tcp_connect_with(addresses, port, |address| async move {
            tokio::net::TcpStream::connect(address).await.map(drop)
        })
        .await
    }

    /// [`Self::tcp_connect`] with the connect itself given, so tests can stand
    /// in for a network that does not answer.
    async fn tcp_connect_with<F, C>(&mut self, addresses: &[IpAddr], port: u16, connect: C) -> bool
    where
        C: Fn(SocketAddr) -> F,
        F: std::future::Future<Output = std::io::Result<()>>,
    {
        let mut check = Check::new(CheckId::TcpConnect, Coverage::Passed);
        // The time limit covers every address together: each is tried with
        // what is left, and none once it is spent.
        let budget = self.request.timeout();
        check.deadline_ms = Some(self.request.timeout_ms());
        check.fields.push(Field::plain("port", port.to_string()));
        // Kept with the check until it is complete: a run canceled part way
        // drops both, so no error names an address the report does not list.
        let mut records = Vec::new();
        let started = Instant::now();
        for &address in addresses {
            let attempt_started = Instant::now();
            let remaining = budget.saturating_sub(started.elapsed());
            let (result, os_error, message) = if remaining.is_zero() {
                (
                    TcpResult::NotAttempted,
                    None,
                    Some(format!(
                        "not tried: the {} s time limit was spent on earlier addresses",
                        budget.as_secs()
                    )),
                )
            } else {
                match tokio::time::timeout(remaining, connect(SocketAddr::new(address, port))).await
                {
                    Ok(Ok(())) => (TcpResult::Connected, None, None),
                    Ok(Err(error)) => (
                        TcpResult::from_io(&error),
                        error.raw_os_error(),
                        Some(error.to_string()),
                    ),
                    Err(_) => (
                        TcpResult::TimedOut,
                        None,
                        Some(format!("no answer within {} ms", remaining.as_millis())),
                    ),
                }
            };
            check.attempts.push(TcpAttempt {
                address,
                port,
                result,
                os_error,
                duration_ms: millis(attempt_started),
            });
            if let Some(message) = message {
                let mut record = ErrorRecord::new(
                    "os",
                    CheckId::TcpConnect,
                    format!("{address}:{port}: {message}"),
                );
                record.os_error = os_error;
                record.code = Some(result.name().to_string());
                records.push(record);
            }
        }
        self.errors.extend(records);
        check.duration_ms = Some(millis(started));

        let results: Vec<TcpResult> = check.attempts.iter().map(|a| a.result).collect();
        if results.contains(&TcpResult::Connected) {
            self.checks.push(check);
            return true;
        }
        if check
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
            return false;
        }

        let all = |r: TcpResult| results.iter().all(|x| *x == r);
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
        } else if results.iter().all(|r| {
            matches!(
                r,
                TcpResult::HostUnreachable | TcpResult::NetworkUnreachable
            )
        }) {
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
            CheckId::TcpConnect,
            format!(
                "No address accepted a TCP connection on port {port}: {}.",
                summary.join(", ")
            ),
        );
        if all(TcpResult::Refused) {
            self.finding(
                Certainty::Suspected,
                CheckId::TcpConnect,
                format!("Nothing is listening on port {port} at those addresses."),
            );
        }
        false
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
        // One connection: no retry.
        context.connect_retry_count = 0;
        context
    }

    async fn connection_attempt(&mut self) -> Option<TdsClient> {
        let mut context = self.client_context();
        let started = Instant::now();
        let mut server = self.attempt_server();
        let mut failed = None;
        match self.localdb_pipe(started).await {
            Some(Ok(pipe)) => {
                server = format!("np:{pipe}");
                // As the client does for LocalDB, which does not encrypt.
                context.encryption_options.mode = EncryptionSetting::PreferOff;
                self.pipe = Some(pipe);
            }
            Some(Err(error)) => failed = Some(error),
            None => {}
        }
        if failed.is_none() && self.request.authentication == Authentication::Integrated {
            let budget = self.request.timeout().saturating_sub(started.elapsed());
            if let Some(spn) = self.requested_spn(budget).await {
                self.supply_spn(&mut context, &spn);
            }
        }
        // One time limit for the attempt: what LocalDB resolving or the SPN
        // lookup used is not given to the client again.
        give_time_left(&mut context, self.request.timeout(), started.elapsed());
        let outcome = match failed {
            Some(error) => Err(error),
            None => {
                let remaining = self.request.timeout().saturating_sub(started.elapsed());
                bounded(
                    remaining,
                    self.request.timeout().as_secs(),
                    // Boxed: the client's connect future is large.
                    Box::pin(TdsConnectionProvider::new().create_client(context, &server, None)),
                )
                .await
            }
        };
        let mut check = Check::new(CheckId::ConnectionAttempt, Coverage::Passed);
        check.duration_ms = Some(millis(started));
        check.deadline_ms = Some(self.request.timeout_ms());
        // The client resolves and connects again itself; only its pre-login,
        // TLS and login phases are reported here.
        let mut phases: Vec<Step> = self
            .recorder
            .steps()
            .into_iter()
            .filter(|s| matches!(s.stage, Stage::Prelogin | Stage::Tls | Stage::Login))
            .collect();
        if outcome.is_err() {
            drop_enclosing_failures(&mut phases);
        }
        check.subphases = phases;
        self.limitation(
            "The connection attempt is made with the mssql-tds client, which reports its \
             pre-login, TLS and login phases; it is not the ODBC driver sqlcmd uses for queries, \
             so TLS and login behavior can differ from a query run.",
        );
        if self.request.authentication == Authentication::Integrated {
            let budget = self.request.timeout().saturating_sub(started.elapsed());
            self.kerberos_evidence(&mut check, budget).await;
            // Looking for the ticket cache is part of the check's time.
            check.duration_ms = Some(millis(started));
        }

        match outcome {
            Ok(client) => {
                if let Some(encryption) = check
                    .subphases
                    .iter()
                    .find(|s| s.stage == Stage::Prelogin)
                    .and_then(|s| s.detail("encryption"))
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
                    packet_size: client.packet_size(),
                    encrypted: client.is_encrypted(),
                    engine_edition: None,
                });
                Some(client)
            }
            Err(error) => {
                let failed_phase = failed_stage(&check.subphases);
                let classified =
                    classify_connection_error(&error, failed_phase, &self.request.authentication);
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
                        "No Kerberos ticket cache was found for this user, so the integrated \
                         login had no ticket to present.",
                    );
                }
                self.authentication = Some(classified.auth);
                self.finding(
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
                None
            }
        }
    }

    /// The host and port the client builds its SPN from, as `mssql-tds` does
    /// for each transport: the TCP host and port, the named pipe's server, and
    /// this machine for shared memory, all non-TCP ones with port 1433.
    fn spn_target(&self) -> Option<(String, u16)> {
        let target = self.target.as_ref()?;
        // A pipe the attempt uses names its server as the client reads it.
        let pipe_server = |pipe: &String| {
            TransportContext::NamedPipe {
                pipe_name: pipe.clone(),
            }
            .get_server_name()
        };
        Some(match target.protocol {
            Protocol::NamedPipe => (
                match &self.pipe {
                    Some(pipe) => pipe_server(pipe),
                    None if target.host == "." => "localhost".to_string(),
                    None => target.host.clone(),
                },
                1433,
            ),
            Protocol::SharedMemory => ("localhost".to_string(), 1433),
            // The client makes it from the instance's resolved pipe.
            Protocol::LocalDb => (pipe_server(self.pipe.as_ref()?), 1433),
            Protocol::Default | Protocol::Tcp | Protocol::Admin => {
                (target.resolvable_host().to_string(), self.port?)
            }
        })
    }
    /// The server the connection attempt is made to: the endpoint the checks
    /// before it confirmed, so the client neither asks SQL Server Browser
    /// again nor connects anywhere else.
    fn attempt_server(&self) -> String {
        if let Some(target) = &self.target {
            let browsed_tcp = matches!(target.protocol, Protocol::Default | Protocol::Tcp)
                && target.instance.is_some()
                && target.port.is_none();
            if let (true, Some(port)) = (browsed_tcp, self.port) {
                return format!("tcp:{},{port}", target.host);
            }
            if let (Protocol::NamedPipe, Some(pipe)) = (target.protocol, &self.pipe) {
                return format!("np:{pipe}");
            }
        }
        connection_server(&self.request.server)
    }

    /// For a LocalDB target on Windows, resolves the instance to its pipe,
    /// starting it if needed. That blocks, so it runs on a thread of its own
    /// for at most what is left of the time limit: neither the limit nor a
    /// cancel waits on it. `None` for any other target.
    async fn localdb_pipe(&self, started: Instant) -> Option<Result<String, Error>> {
        let target = self
            .target
            .as_ref()
            .filter(|t| t.protocol == Protocol::LocalDb)?;
        #[cfg(windows)]
        {
            let instance = target.instance.clone().unwrap_or_default();
            let budget = self.request.timeout().saturating_sub(started.elapsed());
            let seconds = self.request.timeout().as_secs();
            let lookup = instance.clone();
            let resolved = within(budget, move || {
                mssql_tds::connection::transport::resolve_localdb_pipe(&lookup)
            })
            .await;
            Some(resolved.unwrap_or_else(|| {
                Err(Error::TimeoutError(
                    mssql_tds::error::TimeoutErrorType::String(format!(
                        "LocalDB instance {instance} was not resolved or started within {seconds} s"
                    )),
                ))
            }))
        }
        #[cfg(not(windows))]
        {
            let _ = (target, started);
            None
        }
    }

    /// The SPN for integrated authentication, canonicalized once as the client
    /// does. That may block on DNS, so it runs on a thread of its own for at
    /// most `budget`; past it, the host name is used as given.
    async fn requested_spn(&mut self, budget: Duration) -> Option<String> {
        let (host, port) = self.spn_target()?;
        let lookup_host = host.clone();
        let canonicalized = within(budget, move || {
            mssql_tds::security::make_spn_canonicalized(&lookup_host, None, port)
        })
        .await;
        Some(match canonicalized {
            Some(spn) => spn,
            None => {
                self.limitation(&format!(
                    "The SPN's host name could not be canonicalized within the {} s time \
                     limit, so the SPN is requested and shown with the host name as given.",
                    self.request.timeout().as_secs()
                ));
                mssql_tds::security::make_spn(&host, None, port)
            }
        })
    }

    /// Gives the attempt `spn` and reports that same value, in the form the
    /// platform takes (`service@host` for GSSAPI), so the report shows the
    /// SPN the client requested.
    fn supply_spn(&mut self, context: &mut ClientContext, spn: &str) {
        let configured = mssql_tds::security::configured_spn(spn);
        context.server_spn = Some(configured.clone());
        self.spn = Some(configured);
    }

    /// The client's side of Kerberos/SSPI: the SPN the attempt requested and,
    /// off Windows, whether a ticket cache exists. Windows keeps tickets in
    /// the logon session, which needs no cache. The cache may be on a slow
    /// (network) path, so it is looked for on a thread of its own within
    /// `budget`, what is left of the attempt's time limit.
    async fn kerberos_evidence(&mut self, check: &mut Check, budget: Duration) {
        if let Some(spn) = self.spn.clone() {
            check
                .fields
                .push(Field::identifier("spn", spn, Category::Spn));
        }
        if cfg!(windows) {
            check
                .fields
                .push(Field::plain("kerberosTicketCache", "notApplicable"));
        } else if let Some((state, location)) = within(budget, kerberos_ticket_cache).await {
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
                    "Only the default file ticket cache was looked for; a KCM or KEYRING cache \
                     set in krb5.conf is not inspected.",
                );
            }
        } else {
            check
                .fields
                .push(Field::plain("kerberosTicketCache", "notChecked"));
            self.limitation(
                "The Kerberos ticket cache could not be looked for within the time limit.",
            );
        }
        self.limitation(
            "Kerberos encryption types and delegation are not inspected; the SPN is the one the \
             client requests, not proof that it is registered.",
        );
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
                return;
            }
            Ok(Err(error)) => {
                check.coverage = match error {
                    Error::SqlServerError { .. } => Coverage::Diagnosed,
                    _ => Coverage::Inconclusive,
                };
                let message =
                    format!("The session was opened but a minimal query failed: {error}.");
                self.errors
                    .extend(error_records(&error, CheckId::SessionValidation));
                message
            }
            Err(_) => {
                check.coverage = Coverage::Inconclusive;
                let message = format!(
                    "The session was opened but a minimal query had no answer within {} s.",
                    self.request.timeout().as_secs()
                );
                let mut record =
                    ErrorRecord::new("client", CheckId::SessionValidation, message.clone());
                record.code = Some("timedOut".to_string());
                self.errors.push(record);
                message
            }
        };
        check.domain = Some(Domain::specific("target.sqlServer"));
        self.checks.push(check);
        self.finding(Certainty::Confirmed, CheckId::SessionValidation, error);
    }
    fn finish(self, start_unix_ms: u64, duration_ms: u64) -> Report {
        let domain = self
            .checks
            .iter()
            .find(|c| c.coverage.failed())
            .and_then(|c| c.domain.clone());
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
fn dns_result(error: &std::io::Error) -> (&'static str, bool) {
    match error.raw_os_error() {
        Some(11001) => return ("notFound", false), // WSAHOST_NOT_FOUND
        Some(11002) => return ("temporaryFailure", false), // WSATRY_AGAIN
        Some(11004) => return ("noAddresses", false), // WSANO_DATA
        Some(10060) => return ("timedOut", false),
        _ => {}
    }
    let text = error.to_string().to_ascii_lowercase();
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
fn failed_stage(steps: &[Step]) -> Option<Stage> {
    steps
        .iter()
        .filter(|step| !step.ok)
        .max_by_key(|step| step.started)
        .map(|step| step.stage)
}

/// Removes a failed phase that only failed because a phase inside it did (the
/// login around a failed TLS handshake), so the report shows one failure.
fn drop_enclosing_failures(steps: &mut Vec<Step>) {
    let failed: Vec<(Instant, Instant)> = steps
        .iter()
        .filter(|step| !step.ok)
        .map(|step| (step.started, step.ended))
        .collect();
    steps.retain(|step| {
        step.ok
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
        let (coverage, domain) = match category {
            Some("reset") | None => (
                Coverage::Inconclusive,
                Domain::undetermined(&["security.tlsCertificate", "connectivity.network"]),
            ),
            Some(_) => (
                Coverage::Classified,
                Domain::specific("security.tlsCertificate"),
            ),
        };
        let typed = matches!(
            error,
            Error::CertificateExpired | Error::CertificateMismatch | Error::NoServerCertificate
        );
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

/// Gives the client what is left of `limit` after `spent` for its connect and
/// login time limits.
fn give_time_left(context: &mut ClientContext, limit: Duration, spent: Duration) {
    let left = seconds_left(limit, spent);
    context.connect_timeout = left;
    context.login_timeout = Some(left);
}

/// What is left of `limit` after `spent`, in the client's whole seconds,
/// rounded up and at least 1 (0 would mean no limit to the client).
fn seconds_left(limit: Duration, spent: Duration) -> u32 {
    let left = limit.saturating_sub(spent);
    let seconds = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    u32::try_from(seconds).unwrap_or(u32::MAX).max(1)
}

/// A pipe on this computer as the client opens it, `\\.\pipe\...`, not
/// through the network (which can be denied even locally).
fn local_pipe(pipe: String) -> String {
    #[cfg(windows)]
    {
        mssql_tds::connection::transport::localize_pipe_path(&pipe)
    }
    #[cfg(not(windows))]
    {
        pipe
    }
}

/// The client's standard pipe for a named instance: `\\.` for this computer,
/// as the client opens a local pipe.
fn standard_pipe(host: &str, instance: &str) -> String {
    let local = matches!(host, "." | "127.0.0.1" | "::1")
        || host.eq_ignore_ascii_case("(local)")
        || host.eq_ignore_ascii_case("localhost");
    let server = if local { "." } else { host };
    format!("\\\\{server}\\pipe\\MSSQL${instance}\\sql\\query")
}

/// The server the connection attempt is made to. With no protocol prefix the
/// client would try other transports too (shared memory and named pipes on
/// Windows), but the checks before it are over TCP, so it is made over TCP.
fn connection_server(server: &str) -> String {
    match target::parse(server) {
        Ok(target) if target.protocol == Protocol::Default => format!("tcp:{}", server.trim()),
        _ => server.to_string(),
    }
}

/// What the SQL Server Browser lookup ran into, as evidence only: the
/// library's own message also says what to do about it.
fn ssrp_problem(error: &SsrpLookupError) -> String {
    match error {
        SsrpLookupError::Resolve { error, .. } => {
            format!("the server name did not resolve: {error}")
        }
        SsrpLookupError::NoAddresses { .. } => "the server name resolved to no address".into(),
        SsrpLookupError::Socket(_) => "no UDP socket could be opened".into(),
        SsrpLookupError::Send(_) => "no UDP request could be sent".into(),
        SsrpLookupError::Receive(error) => format!("receiving the answer failed: {error}"),
        // No address: it may be one the report does not list, and so not redact.
        SsrpLookupError::NoAnswer { .. } => "no answer".into(),
        SsrpLookupError::TimedOut { .. } => "resolving the server name did not finish".into(),
        SsrpLookupError::InvalidResponse(error) => {
            format!("the answer could not be parsed: {error}")
        }
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
        }
    }

    fn step(stage: Stage, ok: bool, start_ms: u64, end_ms: u64, base: Instant) -> Step {
        Step {
            stage,
            ok,
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
        let typed =
            classify_connection_error(&Error::CertificateExpired, Some(Stage::Tls), &sql_login());
        assert_eq!(typed.coverage, Coverage::Classified);
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
        assert_eq!(failed_stage(&steps), Some(Stage::Tls));
        drop_enclosing_failures(&mut steps);
        let stages: Vec<Stage> = steps.iter().map(|s| s.stage).collect();
        assert_eq!(stages, [Stage::Prelogin, Stage::Tls]);
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
            Some(Domain::specific("client.inputConfiguration"))
        );
        assert_eq!(report.errors[0].code.as_deref(), Some("invalidPort"));
    }

    /// Port 1 on the loopback address refuses at once: name resolution
    /// passes, TCP is diagnosed, and the connection attempt is skipped.
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
    /// Every address shares the check's time limit: one that does not answer
    /// cannot make the check take a time limit per address.
    #[test]
    fn the_time_limit_covers_every_address_together() {
        let mut timed = request("tcp:192.0.2.1,1433", Depth::NetworkReachability);
        timed.login_timeout_seconds = 1;
        let mut diagnosis = Diagnosis::new(timed);
        let addresses: Vec<IpAddr> = ["192.0.2.1", "192.0.2.2", "192.0.2.3"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // A network that never answers.
        runtime.block_on(diagnosis.tcp_connect_with(&addresses, 1433, |_| {
            std::future::pending::<std::io::Result<()>>()
        }));
        let check = &diagnosis.checks[0];
        assert_eq!(check.deadline_ms, Some(1000));
        assert_eq!(check.attempts.len(), 3);
        assert!(
            check.duration_ms.unwrap() < 1600,
            "{} ms for a 1 s limit: {check:#?}",
            check.duration_ms.unwrap()
        );
        let total: u64 = check.attempts.iter().map(|a| a.duration_ms).sum();
        assert!(total < 1600, "{check:#?}");
        // The first address takes the whole limit; the others are not tried.
        assert_eq!(check.attempts[0].result, TcpResult::TimedOut);
        for attempt in &check.attempts[1..] {
            assert_eq!(attempt.result, TcpResult::NotAttempted, "{check:#?}");
        }
        assert_eq!(check.coverage, Coverage::Inconclusive);
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
    /// The attempt goes to the endpoint the checks confirmed: a Browser-found
    /// port or pipe, not the instance name the client would look up again.
    #[test]
    fn the_attempt_uses_the_endpoint_the_checks_confirmed() {
        let attempt = |server: &str, port: Option<u16>, pipe: Option<&str>| {
            let mut diagnosis = Diagnosis::new(request(server, Depth::ConnectionAttempt));
            diagnosis.target = Some(target::parse(server).unwrap());
            diagnosis.port = port;
            diagnosis.pipe = pipe.map(str::to_string);
            diagnosis.attempt_server()
        };
        assert_eq!(
            attempt("db01\\SQL2022", Some(50001), None),
            "tcp:db01,50001"
        );
        assert_eq!(
            attempt("tcp:db01\\SQL2022", Some(50001), None),
            "tcp:db01,50001"
        );
        assert_eq!(
            attempt("db01\\SQL2022,1500", Some(1500), None),
            "tcp:db01\\SQL2022,1500"
        );
        assert_eq!(attempt("db01", Some(1433), None), "tcp:db01");
        assert_eq!(
            attempt("np:db01\\SQL2022", None, Some("\\\\db01\\pipe\\x")),
            "np:\\\\db01\\pipe\\x"
        );
        assert_eq!(
            standard_pipe("db01", "X"),
            "\\\\db01\\pipe\\MSSQL$X\\sql\\query"
        );
        assert_eq!(
            standard_pipe("LocalHost", "X"),
            "\\\\.\\pipe\\MSSQL$X\\sql\\query"
        );
    }
    /// `np:host\instance` keeps its instance and gets a pipe, from SQL Server
    /// Browser or, with no answer, the instance's standard pipe.
    #[test]
    fn a_named_pipe_instance_gets_its_pipe() {
        let report = run(request(
            "np:127.0.0.1\\NoSuchDiagnosePipe",
            Depth::EndpointResolution,
        ));
        let lookup = report
            .checks
            .iter()
            .find(|c| c.id == CheckId::InstanceResolution)
            .unwrap();
        assert_eq!(lookup.skip, None, "{report:#?}");
        assert_eq!(lookup.field("instance"), Some("NoSuchDiagnosePipe"));
        assert_eq!(lookup.field("pipeSource"), Some("standard"), "{report:#?}");
        assert_eq!(
            lookup.field("pipe"),
            Some("\\\\.\\pipe\\MSSQL$NoSuchDiagnosePipe\\sql\\query")
        );
    }
    /// A pipe naming this computer, as SQL Server Browser returns it, is
    /// opened locally, as the client does.
    #[cfg(windows)]
    #[test]
    fn a_pipe_on_this_computer_is_opened_locally() {
        let computer = std::env::var("COMPUTERNAME").unwrap();
        assert_eq!(
            local_pipe(format!("\\\\{computer}\\pipe\\MSSQL$X\\sql\\query")),
            "\\\\.\\pipe\\MSSQL$X\\sql\\query"
        );
        assert_eq!(
            local_pipe("\\\\db01\\pipe\\sql\\query".to_string()),
            "\\\\db01\\pipe\\sql\\query"
        );
    }
    /// The SPN reported is the one supplied to the client, in the form the
    /// platform takes (GSSAPI's `service@host` off Windows).
    #[test]
    fn the_reported_spn_is_the_one_supplied() {
        let mut diagnosis = Diagnosis::new(request("tcp:db01,1433", Depth::ConnectionAttempt));
        let mut context = diagnosis.client_context();
        let spn = mssql_tds::security::make_spn("db01.contoso.com", None, 1433);
        diagnosis.supply_spn(&mut context, &spn);
        let configured = mssql_tds::security::configured_spn(&spn);
        assert_eq!(context.server_spn.as_deref(), Some(configured.as_str()));
        assert_eq!(diagnosis.spn.as_deref(), Some(configured.as_str()));
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
    /// The attempt opens one connection, without a retry, and gets only the
    /// time the steps before it left.
    #[test]
    fn the_attempt_opens_one_connection_in_the_time_left() {
        let diagnosis = Diagnosis::new(request("tcp:db01,1433", Depth::ConnectionAttempt));
        let mut context = diagnosis.client_context();
        assert_eq!(context.connect_retry_count, 0);
        give_time_left(
            &mut context,
            Duration::from_secs(15),
            Duration::from_millis(9_200),
        );
        assert_eq!(context.connect_timeout, 6);
        assert_eq!(context.login_timeout, Some(6));
    }
    /// LocalDB resolving and the connection share one time limit: the client
    /// gets what is left, in whole seconds, never 0 (no limit).
    #[test]
    fn the_client_gets_the_time_left() {
        let s = Duration::from_secs;
        assert_eq!(seconds_left(s(15), s(0)), 15);
        assert_eq!(seconds_left(s(15), s(10)), 5);
        assert_eq!(seconds_left(s(15), Duration::from_millis(10_500)), 5);
        assert_eq!(seconds_left(s(15), s(15)), 1);
        assert_eq!(seconds_left(s(15), s(20)), 1);
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
    /// A TCP check canceled after a failed address leaves no error behind:
    /// the canceled report does not list that address, so it is not redacted.
    #[test]
    fn a_tcp_check_canceled_part_way_leaves_no_error() {
        let mut diagnosis =
            Diagnosis::new(request("tcp:192.0.2.1,1433", Depth::NetworkReachability));
        let addresses: Vec<IpAddr> = ["192.0.2.1", "192.0.2.2"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        let first: IpAddr = addresses[0];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // The first address refuses; the second never answers, and the run
        // is canceled while it waits.
        let finished = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(100),
                diagnosis.tcp_connect_with(&addresses, 1433, |to: SocketAddr| async move {
                    if to.ip() == first {
                        Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
                    } else {
                        std::future::pending().await
                    }
                }),
            )
            .await
        });
        assert!(finished.is_err(), "the check was canceled");
        assert!(diagnosis.checks.is_empty());
        assert!(diagnosis.errors.is_empty(), "{:#?}", diagnosis.errors);
    }
    #[test]
    fn the_spn_follows_the_transport_the_client_uses() {
        let spn = |server: &str, port: Option<u16>| {
            let mut diagnosis = Diagnosis::new(request(server, Depth::ConnectionAttempt));
            diagnosis.target = Some(target::parse(server).unwrap());
            diagnosis.port = port;
            diagnosis.spn_target()
        };
        assert_eq!(
            spn("tcp:db01,1500", Some(1500)),
            Some(("db01".to_string(), 1500))
        );
        assert_eq!(spn("tcp:db01", None), None, "no confirmed port yet");
        assert_eq!(
            spn("np:\\\\db01\\pipe\\sql\\query", None),
            Some(("db01".to_string(), 1433))
        );
        assert_eq!(
            spn("\\\\.\\pipe\\sql\\query", None),
            Some(("localhost".to_string(), 1433))
        );
        assert_eq!(spn("lpc:.", None), Some(("localhost".to_string(), 1433)));

        // A pipe the attempt uses names the server, as the client reads it:
        // the Browser's pipe for np:host\instance, LocalDB's resolved pipe.
        let with_pipe = |server: &str, pipe: Option<&str>| {
            let mut diagnosis = Diagnosis::new(request(server, Depth::ConnectionAttempt));
            diagnosis.target = Some(target::parse(server).unwrap());
            diagnosis.pipe = pipe.map(str::to_string);
            diagnosis.spn_target()
        };
        assert_eq!(
            with_pipe(
                "np:db01\\SQL2022",
                Some("\\\\DB01-NODE\\pipe\\MSSQL$SQL2022\\sql\\query")
            ),
            Some(("DB01-NODE".to_string(), 1433))
        );
        assert_eq!(
            with_pipe(
                "(localdb)\\MSSQLLocalDB",
                Some("\\\\.\\pipe\\LOCALDB#AB12\\tsql\\query")
            ),
            Some(("localhost".to_string(), 1433))
        );
        assert_eq!(
            with_pipe("(localdb)\\MSSQLLocalDB", None),
            None,
            "not before the instance is resolved"
        );
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
            assert!(
                check.skip.is_none() || check.skip == Some(Skip::Canceled),
                "{check:#?}"
            );
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
    /// With no protocol prefix every check, the connection attempt included,
    /// is made over TCP; other forms are passed on as given.
    #[test]
    fn a_target_without_a_prefix_is_connected_to_over_tcp() {
        assert_eq!(connection_server("db01"), "tcp:db01");
        assert_eq!(
            connection_server(" db01\\inst,1500 "),
            "tcp:db01\\inst,1500"
        );
        assert_eq!(connection_server("(local)"), "tcp:(local)");
        for given in [
            "tcp:db01,1433",
            "np:\\\\db01\\pipe\\sql\\query",
            "lpc:.",
            "admin:db01",
            "(localdb)\\MSSQLLocalDB",
            "db,abc",
        ] {
            assert_eq!(connection_server(given), given);
        }
    }
    /// On Windows, sqlcmd without a prefix can use other transports, so a
    /// TCP failure says that only TCP was checked.
    #[test]
    fn an_unreachable_target_without_a_prefix_says_only_tcp_was_checked() {
        let only_tcp = |report: &Report| {
            report
                .limitations
                .iter()
                .any(|l| l.contains("checks TCP only"))
        };
        let report = run(request("127.0.0.1,1", Depth::NetworkReachability));
        assert_eq!(only_tcp(&report), cfg!(windows), "{report:#?}");
        let report = run(request("tcp:127.0.0.1,1", Depth::NetworkReachability));
        assert!(!only_tcp(&report));
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
    /// The Browser's failure is evidence only: the library's message, which
    /// also gives advice, is not copied into the report.
    #[test]
    fn a_browser_failure_is_described_without_advice() {
        use mssql_tds::ssrp::SsrpLookupError as E;
        let failures = [
            E::Send(None),
            E::NoAnswer {
                timeout_ms: 2000,
                address: "10.0.0.1".to_string(),
            },
            E::TimedOut {
                server: "db01".to_string(),
                timeout_ms: 2000,
            },
            E::Socket(None),
            E::NoAddresses {
                server: "db01".to_string(),
            },
        ];
        for failure in &failures {
            let problem = ssrp_problem(failure);
            assert!(!problem.is_empty());
            for advice in ["Verify", "Ensure", "Contact", "Check "] {
                assert!(!problem.contains(advice), "{problem}");
            }
            // Nor an identifier the report may not list, and so not redact.
            for identifier in ["10.0.0.1", "db01"] {
                assert!(!problem.contains(identifier), "{problem}");
            }
        }
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
}

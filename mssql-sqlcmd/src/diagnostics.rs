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
//! connection input check only: the checks after it are reported as skipped,
//! not checked by this build. Checks run best effort: a
//! failed check skips only the checks that need its result, and each skipped
//! check names what blocked it. Each check
//! reports a coverage state (`passed`, `diagnosed`, `classified`,
//! `inconclusive`, `skipped`, `notApplicable`), its duration on a monotonic
//! clock, its time limit, and what it found. The [`Report`] adds the findings (confirmed,
//! suspected, informational), the ordered error chain with its stable
//! identifiers, the specialist domain for a handoff, the execution status, the
//! diagnostic outcome and the exit code. [`report`] renders it as text or JSON,
//! for review before sharing: database and user names become labels unless
//! local detail is requested ([`redact`]).
//!
//! sqlcmd makes no network call of its own: the checks that connect will
//! read the evidence of the connection attempt `mssql-tds` makes. The
//! password is never part of the report.

pub mod redact;
pub mod report;
pub mod target;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use redact::Category;
use target::Target;

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

/// How long SQL Server Browser has to answer.
const BROWSER_TIMEOUT_MS: u64 = 2000;

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

/// One check as it ran.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub id: CheckId,
    pub coverage: Coverage,
    pub duration_ms: Option<u64>,
    /// How long the check was allowed to take, when it had a time limit.
    pub deadline_ms: Option<u64>,
    pub fields: Vec<Field>,
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
            skip: None,
            domain: None,
        }
    }

    fn skipped(id: CheckId, skip: Skip) -> Self {
        Self {
            skip: Some(skip),
            ..Self::new(id, Coverage::Skipped)
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
    } else if canceled() {
        diagnosis.canceled();
    } else {
        diagnosis.execute();
    }
    diagnosis.finish(
        start_unix_ms,
        u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    )
}

/// The state of one diagnosis while it runs.
struct Diagnosis {
    request: Request,
    /// Whether Connection input parses as the client does on Windows, which
    /// alone takes LocalDB, named pipes and shared memory: this platform's,
    /// and either in tests.
    windows: bool,
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

impl Diagnosis {
    fn new(request: Request) -> Self {
        Self {
            request,
            windows: cfg!(windows),
            target: None,
            checks: Vec::new(),
            findings: Vec::new(),
            errors: Vec::new(),
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
            quotes_message: false,
        });
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

    /// This build checks the connection input only: the checks after it are
    /// reported as not checked by this build.
    fn execute(&mut self) {
        if self.connection_input().is_some() {
            self.skip_rest(CheckId::ConnectionInput, &Skip::NotInThisBuild);
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
        // Canceled before any check ran.
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

    /// This build checks the connection input only: every deeper check at the
    /// depth is reported as not checked by this build, and nothing is
    /// evaluated past the input.
    #[test]
    fn the_checks_past_the_input_are_not_run_by_this_build() {
        let report = run(request("tcp:db01,1433", Depth::SessionValidation));
        assert_eq!(report.checks.len(), CheckId::ORDER.len(), "{report:#?}");
        assert_eq!(report.checks[0].coverage, Coverage::Passed);
        for check in &report.checks[1..] {
            assert_eq!(check.coverage, Coverage::Skipped, "{check:#?}");
            assert_eq!(check.skip, Some(Skip::NotInThisBuild), "{check:#?}");
        }
        assert_eq!(report.execution_status, ExecutionStatus::Completed);
        assert_eq!(report.outcome, DiagnosticOutcome::NotEvaluated);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(
            report.deepest_phase_evaluated(),
            Some(Depth::ConnectionInput)
        );
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! JSON output for `--format json`.
//!
//! Native sqlcmd runs the batches as usual and, instead of printing text, hands
//! each piece of output to a [`JsonDocument`]: the batches it sends, result
//! sets and their rows, row counts, and server messages. When sqlcmd exits it
//! calls [`JsonDocument::render`] with the connection details, the exit code
//! and whether the run was canceled or rejected, and prints the one JSON
//! document that returns. `serde_json` writes it, indented, with each row and
//! column description on one line:
//!
//! ```json
//! {
//!   "contractVersion": "1.0",
//!   "sqlcmd": {
//!     "version": "18.7.0001.1",
//!     "platform": "win-x64"
//!   },
//!   "connection": {
//!     "server": "tcp:localhost,1433",
//!     "database": "master",
//!     "authentication": "SqlPassword",
//!     "encrypt": true,
//!     "serverVersion": "17.00.1000",
//!     "connectMs": 12
//!   },
//!   "startTime": "2026-10-03T02:40:11.483Z",
//!   "durationMs": 42,
//!   "executionStatus": "completed",
//!   "operationOutcome": "succeeded",
//!   "exitCode": 0,
//!   "output": [
//!     {
//!       "type": "batch",
//!       "index": 1,
//!       "durationMs": 4,
//!       "text": "SELECT id, name FROM t"
//!     },
//!     {
//!       "type": "resultSet",
//!       "columns": [
//!         { "ordinal": 0, "name": "id", "driverType": "int", "precision": 10, "scale": 0, "nullable": false },
//!         { "ordinal": 1, "name": "name", "driverType": "nvarchar", "size": 50, "nullable": true }
//!       ],
//!       "rows": [
//!         ["1", "a"],
//!         ["2", null]
//!       ],
//!       "rowsAffected": 2
//!     }
//!   ]
//! }
//! ```
//!
//! The document follows the `format json` contract of the sqlcmd
//! specification (`contractVersion` 1.0):
//!
//! - `executionStatus` says whether sqlcmd carried out the run (`completed`,
//!   `canceled`, `invalidInvocation`; `partial` and `failed` are defined by
//!   the contract but not produced by native sqlcmd), and `operationOutcome`
//!   what the run's operation came to (`succeeded`, `failed`, `canceled`,
//!   `notExecuted`). sqlcmd's exit code is unchanged by `--format json`.
//! - Messages and errors carry what the console shows for them: `number`
//!   (Msg), `severity` (Level), `state`, `server`, `procedure`, `line`, the
//!   reporting `source` when it is not the server (the driver, or sqlcmd), and
//!   the ODBC `sqlState`, then their `text`. No failure category is derived
//!   from them; the SQLSTATE and number are the stable fields to classify by.
//! - Columns are described as the driver describes them: `ordinal` and `name`
//!   always (`null` for an unnamed column), and `driverType`, `size`,
//!   `precision`, `scale` and `nullable` when the driver exposes them. Type
//!   names are not normalized.
//! - Values are strings, exactly as sqlcmd would print them (the same `-R`,
//!   `-k`, `-y` and `-Y` treatment), so no precision is
//!   lost and every SQL type has a representation. SQL `NULL` is JSON `null`,
//!   which keeps it distinct from the string `"NULL"`. The output of `FOR
//!   JSON` and `FOR XML` is ordinary string values, row by row, as the server
//!   returned them.
//!
//! `output` lists entries in the order they began: a result set takes its place
//! when its columns arrive, and all its rows stay inside it. A message the
//! server sends while a result set's rows are still arriving therefore follows
//! that whole result set, not the row it arrived after.
//!
//! The whole document is held in memory until `render`, because it starts with
//! details only known at exit (the exit code, the duration) and must stay one
//! valid JSON value even when sqlcmd stops part-way. Peak memory is a few times
//! the size of the output; text output remains the way to stream very large
//! results.

use std::io;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::ser::{Formatter, PrettyFormatter};

/// Version of the document's contract, `major.minor`. The major version
/// changes only for an incompatible change; a minor version may add optional
/// fields, output entry types or enum values.
pub const CONTRACT_VERSION: &str = "1.0";

/// The runtime this library was built for, as a NuGet runtime identifier. A
/// target outside the supported matrix gets the portable identifier of its
/// operating system (`win`, `osx` or `linux`), or `unknown` for any other
/// operating system: a placeholder, not a supported runtime.
pub const PLATFORM: &str = platform();

const fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        if cfg!(target_arch = "x86_64") {
            "win-x64"
        } else if cfg!(target_arch = "x86") {
            "win-x86"
        } else if cfg!(target_arch = "aarch64") {
            "win-arm64"
        } else {
            "win"
        }
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "osx-arm64"
        } else if cfg!(target_arch = "x86_64") {
            "osx-x64"
        } else {
            "osx"
        }
    } else if cfg!(target_os = "linux") {
        let musl = cfg!(target_env = "musl");
        if cfg!(target_arch = "aarch64") {
            if musl {
                "linux-musl-arm64"
            } else {
                "linux-arm64"
            }
        } else if cfg!(target_arch = "x86_64") {
            if musl { "linux-musl-x64" } else { "linux-x64" }
        } else {
            "linux"
        }
    } else {
        "unknown"
    }
}

/// Connection details for the document's `connection` object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connection {
    /// The server named on the command line; `null` when unknown.
    pub server: Option<String>,
    /// The database in use once connected, or the one named on the command
    /// line; `null` when unknown.
    pub database: Option<String>,
    /// How sqlcmd authenticated, e.g. `Integrated` or `SqlPassword`.
    pub authentication: Option<String>,
    /// Whether the connection was encrypted.
    pub encrypt: bool,
}

/// How the run ended, besides its exit code. With the exit code it decides
/// `executionStatus` and `operationOutcome`. No failure category is derived:
/// each error carries the number, state and SQLSTATE the server or driver gave
/// it, which callers can classify by, and new error numbers need no update here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunEnd {
    /// sqlcmd ran to its end; the exit code says whether the work succeeded.
    #[default]
    Finished,
    /// The run was canceled, e.g. with Ctrl+C.
    Canceled,
    /// The command line was rejected, so nothing ran.
    InvalidInvocation,
}

/// `executionStatus` and `operationOutcome` for a run that ended as `end` with
/// `exit_code`.
fn status_and_outcome(end: RunEnd, exit_code: i32) -> (&'static str, &'static str) {
    match end {
        RunEnd::Finished if exit_code == 0 => ("completed", "succeeded"),
        RunEnd::Finished => ("completed", "failed"),
        RunEnd::Canceled => ("canceled", "canceled"),
        RunEnd::InvalidInvocation => ("invalidInvocation", "notExecuted"),
    }
}

/// A result-set column, as the driver describes it. Only `name` is always
/// known; everything else is rendered only when the driver exposed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Column {
    /// `None` for an unnamed column, rendered as JSON `null`.
    pub name: Option<String>,
    /// The type name exactly as the driver reports it, e.g. `nvarchar` or
    /// `int identity`.
    pub driver_type: Option<String>,
    /// Size in characters (character types) or bytes (binary types).
    pub size: Option<i64>,
    pub precision: Option<i32>,
    pub scale: Option<i32>,
    pub nullable: Option<bool>,
}

/// A message or error, with what the console shows for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    /// `true` for an error, `false` for an informational message such as
    /// `PRINT`.
    pub is_error: bool,
    /// The error number (`Msg`), 0 when there is none.
    pub number: i32,
    /// The severity (`Level`).
    pub severity: i32,
    pub state: i32,
    /// The server that sent it, as the console's `Server` shows it.
    pub server: Option<String>,
    /// The stored procedure the message came from, when there was one.
    pub procedure: Option<String>,
    /// The line the server reported, when it reported one.
    pub line: Option<i32>,
    /// Who reported it, when it did not come from the server: the driver (e.g.
    /// `Microsoft ODBC Driver 18 for SQL Server`) or `Sqlcmd` itself.
    pub source: Option<String>,
    /// The ODBC SQLSTATE, e.g. `28000` for a refused login, when there is one.
    pub sql_state: Option<String>,
    pub text: String,
}

/// Why a call could not be applied to the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// A row or count arrived when no result set was open.
    NoResultSet,
    /// A result set was started while another was still open.
    ResultSetOpen,
    /// A row's value count differs from the result set's column count.
    ColumnCountMismatch { expected: usize, actual: usize },
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::NoResultSet => write!(f, "no result set is open"),
            FormatError::ResultSetOpen => write!(f, "a result set is already open"),
            FormatError::ColumnCountMismatch { expected, actual } => {
                write!(
                    f,
                    "a row has {actual} values but the result set has {expected} columns"
                )
            }
        }
    }
}

impl std::error::Error for FormatError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Output {
    Batch {
        index: u32,
        text: Option<String>,
        started: Instant,
        duration_ms: Option<u64>,
    },
    ResultSet {
        columns: Vec<Column>,
        rows: Vec<Vec<Option<String>>>,
        rows_affected: Option<i64>,
    },
    RowsAffected(i64),
    Message(Message),
}

/// Collects one invocation's output, in order, and renders it as JSON.
#[derive(Debug)]
pub struct JsonDocument {
    /// When the document was created, which native sqlcmd does at startup,
    /// before it connects: wall-clock time for `startTime`, and a monotonic
    /// one for the durations.
    start_unix_ms: u64,
    start: Instant,
    connecting: Option<Instant>,
    server_version: Option<String>,
    connect_ms: Option<u64>,
    output: Vec<Output>,
    /// Index in `output` of the result set rows are added to. A message can
    /// arrive between two rows of the same result set, so this is not always
    /// the last entry.
    current_result_set: Option<usize>,
    /// Index in `output` of the batch that is running.
    current_batch: Option<usize>,
    batches: u32,
}

impl Default for JsonDocument {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonDocument {
    /// Creates an empty document, starting the clock for `startTime` and
    /// `durationMs`.
    pub fn new() -> Self {
        let start_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        Self::starting_at(start_unix_ms, Instant::now())
    }

    /// Creates an empty document whose clock started at the given times.
    fn starting_at(start_unix_ms: u64, start: Instant) -> Self {
        Self {
            start_unix_ms,
            start,
            connecting: None,
            server_version: None,
            connect_ms: None,
            output: Vec::new(),
            current_result_set: None,
            current_batch: None,
            batches: 0,
        }
    }

    /// Notes that sqlcmd started connecting. A reconnect (`:CONNECT`) that
    /// fails keeps the live connection, so what that connection reported stays
    /// until [`connected`](Self::connected) replaces it.
    pub fn connecting(&mut self) {
        self.connecting_at(Instant::now());
    }

    fn connecting_at(&mut self, now: Instant) {
        self.connecting = Some(now);
    }

    /// Notes that sqlcmd connected, and the version the server reported.
    /// `connectMs` runs from the matching [`connecting`](Self::connecting),
    /// or from the start if there was none. A later connection (`:CONNECT`)
    /// replaces both.
    pub fn connected(&mut self, server_version: Option<String>) {
        self.connected_at(server_version, Instant::now());
    }

    fn connected_at(&mut self, server_version: Option<String>, now: Instant) {
        let from = self.connecting.take().unwrap_or(self.start);
        self.connect_ms = Some(millis_between(from, now));
        self.server_version = server_version;
    }

    /// Starts a batch sent to the server. `text` is the batch as sent, or
    /// `None` to leave it out (sqlcmd includes it only with `-e`). A batch
    /// still running is ended first, and a result set still open is closed
    /// without a count, so the new batch's rows cannot land in it.
    pub fn begin_batch(&mut self, text: Option<String>) {
        self.begin_batch_at(text, Instant::now());
    }

    fn begin_batch_at(&mut self, text: Option<String>, now: Instant) {
        self.end_batch_at(now);
        self.current_result_set = None;
        self.batches += 1;
        self.current_batch = Some(self.output.len());
        self.output.push(Output::Batch {
            index: self.batches,
            text,
            started: now,
            duration_ms: None,
        });
    }

    /// Ends the running batch, recording how long it took. Does nothing when
    /// no batch is running.
    pub fn end_batch(&mut self) {
        self.end_batch_at(Instant::now());
    }

    fn end_batch_at(&mut self, now: Instant) {
        if let Some(Output::Batch {
            started,
            duration_ms,
            ..
        }) = self
            .current_batch
            .take()
            .and_then(|index| self.output.get_mut(index))
        {
            *duration_ms = Some(millis_between(*started, now));
        }
    }

    /// Starts a result set; the rows that follow belong to it. The one before
    /// must have been ended, or it could never get its count: starting one
    /// while another is open is rejected, and the open one is closed without
    /// a count, so no later row can land in it.
    pub fn begin_result_set(&mut self, columns: Vec<Column>) -> Result<(), FormatError> {
        if self.current_result_set.take().is_some() {
            return Err(FormatError::ResultSetOpen);
        }
        self.current_result_set = Some(self.output.len());
        self.output.push(Output::ResultSet {
            columns,
            rows: Vec::new(),
            rows_affected: None,
        });
        Ok(())
    }

    /// Adds a row to the current result set. `None` is SQL `NULL`.
    pub fn add_row(&mut self, values: Vec<Option<String>>) -> Result<(), FormatError> {
        let Some(Output::ResultSet { columns, rows, .. }) = self
            .current_result_set
            .and_then(|index| self.output.get_mut(index))
        else {
            return Err(FormatError::NoResultSet);
        };
        if values.len() != columns.len() {
            return Err(FormatError::ColumnCountMismatch {
                expected: columns.len(),
                actual: values.len(),
            });
        }
        rows.push(values);
        Ok(())
    }

    /// Ends the current result set with the count sqlcmd reports for it, or
    /// none (`SET NOCOUNT ON`). Rows can no longer be added to it.
    pub fn end_result_set(&mut self, rows_affected: Option<i64>) -> Result<(), FormatError> {
        let Some(Output::ResultSet {
            rows_affected: count,
            ..
        }) = self
            .current_result_set
            .take()
            .and_then(|index| self.output.get_mut(index))
        else {
            return Err(FormatError::NoResultSet);
        };
        *count = rows_affected;
        Ok(())
    }

    /// Records the "(n rows affected)" of a statement that returned no result
    /// set.
    pub fn add_rows_affected(&mut self, count: i64) {
        self.output.push(Output::RowsAffected(count));
    }

    /// Records a server message or error.
    pub fn add_message(&mut self, message: Message) {
        self.output.push(Output::Message(message));
    }

    /// Renders the whole document, ending with a newline.
    ///
    /// `end` says whether the run was canceled or rejected; otherwise the exit
    /// code decides whether the operation succeeded.
    pub fn render(
        &self,
        version: &str,
        connection: &Connection,
        exit_code: i32,
        end: RunEnd,
    ) -> Result<String, serde_json::Error> {
        self.render_at(version, connection, exit_code, end, Instant::now())
    }

    fn render_at(
        &self,
        version: &str,
        connection: &Connection,
        exit_code: i32,
        end: RunEnd,
        now: Instant,
    ) -> Result<String, serde_json::Error> {
        let (execution_status, operation_outcome) = status_and_outcome(end, exit_code);
        let document = DocumentView {
            contract_version: CONTRACT_VERSION,
            sqlcmd: SqlcmdView {
                version,
                platform: PLATFORM,
            },
            connection: ConnectionView {
                server: connection.server.as_deref(),
                database: connection.database.as_deref(),
                authentication: connection.authentication.as_deref(),
                encrypt: connection.encrypt,
                server_version: self.server_version.as_deref(),
                connect_ms: self.connect_ms,
            },
            start_time: utc_timestamp(self.start_unix_ms),
            duration_ms: millis_between(self.start, now),
            execution_status,
            operation_outcome,
            exit_code,
            output: self
                .output
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    // The batch still running is timed up to the render.
                    let running = (self.current_batch == Some(index)).then_some(now);
                    OutputView::new(item, running)
                })
                .collect(),
        };
        let mut out = Vec::new();
        let mut serializer = serde_json::Serializer::with_formatter(&mut out, Layout::default());
        document.serialize(&mut serializer)?;
        out.push(b'\n');
        // serde_json writes only UTF-8.
        String::from_utf8(out).map_err(serde::ser::Error::custom)
    }
}

/// How deep a row or a column description sits: document, `output`, an
/// entry, its `rows` or `columns`, then the row or column itself.
const ONE_LINE_DEPTH: usize = 5;

/// serde_json's indented layout, except that a row or a column description
/// (anything nested `ONE_LINE_DEPTH` deep or deeper) is written on one line:
/// `["master", "1", null]`, `{ "ordinal": 0, "name": "id" }`.
#[derive(Default)]
struct Layout {
    pretty: PrettyFormatter<'static>,
    depth: usize,
}

impl Layout {
    fn one_line(&self) -> bool {
        self.depth >= ONE_LINE_DEPTH
    }
}

impl Formatter for Layout {
    fn begin_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.depth += 1;
        if self.one_line() {
            writer.write_all(b"[")
        } else {
            self.pretty.begin_array(writer)
        }
    }

    fn end_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        let result = if self.one_line() {
            writer.write_all(b"]")
        } else {
            self.pretty.end_array(writer)
        };
        self.depth -= 1;
        result
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if !self.one_line() {
            self.pretty.begin_array_value(writer, first)
        } else if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn end_array_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if self.one_line() {
            Ok(())
        } else {
            self.pretty.end_array_value(writer)
        }
    }

    fn begin_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.depth += 1;
        if self.one_line() {
            writer.write_all(b"{")
        } else {
            self.pretty.begin_object(writer)
        }
    }

    fn end_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        let result = if self.one_line() {
            writer.write_all(b" }")
        } else {
            self.pretty.end_object(writer)
        };
        self.depth -= 1;
        result
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if !self.one_line() {
            self.pretty.begin_object_key(writer, first)
        } else if first {
            writer.write_all(b" ")
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if self.one_line() {
            writer.write_all(b": ")
        } else {
            self.pretty.begin_object_value(writer)
        }
    }

    fn end_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if self.one_line() {
            Ok(())
        } else {
            self.pretty.end_object_value(writer)
        }
    }
}

// The document as serde writes it. The views borrow from `JsonDocument`, so
// rendering copies no values; field order is the contract's order, and
// optional fields are left out when absent.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentView<'a> {
    contract_version: &'static str,
    sqlcmd: SqlcmdView<'a>,
    connection: ConnectionView<'a>,
    start_time: String,
    duration_ms: u64,
    execution_status: &'static str,
    operation_outcome: &'static str,
    exit_code: i32,
    output: Vec<OutputView<'a>>,
}

#[derive(Serialize)]
struct SqlcmdView<'a> {
    version: &'a str,
    platform: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionView<'a> {
    server: Option<&'a str>,
    database: Option<&'a str>,
    authentication: Option<&'a str>,
    encrypt: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    connect_ms: Option<u64>,
}

/// One `output` entry, tagged with its `type`.
#[derive(Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum OutputView<'a> {
    Batch {
        index: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<&'a str>,
    },
    ResultSet {
        columns: Vec<ColumnView<'a>>,
        rows: &'a [Vec<Option<String>>],
        #[serde(skip_serializing_if = "Option::is_none")]
        rows_affected: Option<i64>,
    },
    RowsAffected {
        count: i64,
    },
    Message(MessageView<'a>),
    Error(MessageView<'a>),
}

impl<'a> OutputView<'a> {
    /// The entry for `item`; `now` is given for the batch still running.
    fn new(item: &'a Output, now: Option<Instant>) -> Self {
        match item {
            Output::Batch {
                index,
                text,
                started,
                duration_ms,
            } => OutputView::Batch {
                index: *index,
                duration_ms: duration_ms.or_else(|| now.map(|now| millis_between(*started, now))),
                text: text.as_deref(),
            },
            Output::ResultSet {
                columns,
                rows,
                rows_affected,
            } => OutputView::ResultSet {
                columns: columns
                    .iter()
                    .enumerate()
                    .map(|(ordinal, column)| ColumnView::new(ordinal, column))
                    .collect(),
                rows,
                rows_affected: *rows_affected,
            },
            Output::RowsAffected(count) => OutputView::RowsAffected { count: *count },
            Output::Message(message) if message.is_error => {
                OutputView::Error(MessageView::new(message))
            }
            Output::Message(message) => OutputView::Message(MessageView::new(message)),
        }
    }
}

/// A `columns` entry: `ordinal` and `name` always, the rest only when the
/// driver exposed it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ColumnView<'a> {
    ordinal: usize,
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    driver_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    precision: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scale: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nullable: Option<bool>,
}

impl<'a> ColumnView<'a> {
    fn new(ordinal: usize, column: &'a Column) -> Self {
        Self {
            ordinal,
            name: column.name.as_deref(),
            driver_type: column.driver_type.as_deref(),
            size: column.size,
            precision: column.precision,
            scale: column.scale,
            nullable: column.nullable,
        }
    }
}

/// A message or error: the fields the console shows (Msg, Level, State,
/// Server, Procedure, Line, and who reported it), then the SQLSTATE and the
/// text.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageView<'a> {
    number: i32,
    severity: i32,
    state: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    procedure: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sql_state: Option<&'a str>,
    text: &'a str,
}

impl<'a> MessageView<'a> {
    fn new(message: &'a Message) -> Self {
        Self {
            number: message.number,
            severity: message.severity,
            state: message.state,
            server: message.server.as_deref(),
            procedure: message.procedure.as_deref(),
            line: message.line,
            source: message.source.as_deref(),
            sql_state: message.sql_state.as_deref(),
            text: &message.text,
        }
    }
}

fn millis_between(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_millis()).unwrap_or(u64::MAX)
}

/// Formats milliseconds since the Unix epoch as an RFC 3339 UTC timestamp with
/// milliseconds, e.g. `2026-10-03T02:40:11.483Z`.
fn utc_timestamp(unix_ms: u64) -> String {
    let millis = unix_ms % 1000;
    let seconds = unix_ms / 1000;
    let (hour, minute, second) = (seconds / 3600 % 24, seconds / 60 % 60, seconds % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 2026-10-03T02:40:11.483Z
    const START_UNIX_MS: u64 = 1_790_995_211_483;

    fn connection() -> Connection {
        Connection {
            server: Some("localhost".to_string()),
            database: Some("master".to_string()),
            authentication: Some("SqlPassword".to_string()),
            encrypt: true,
        }
    }

    fn column(name: &str) -> Column {
        Column {
            name: Some(name.to_string()),
            driver_type: Some("int".to_string()),
            ..Column::default()
        }
    }

    fn message(is_error: bool, text: &str) -> Message {
        Message {
            is_error,
            number: if is_error { 50000 } else { 0 },
            state: 1,
            severity: if is_error { 16 } else { 0 },
            text: text.to_string(),
            ..Message::default()
        }
    }

    /// A document whose clock started at a known time, and that instant.
    fn document() -> (JsonDocument, Instant) {
        let start = Instant::now();
        (JsonDocument::starting_at(START_UNIX_MS, start), start)
    }

    fn after(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    /// The `output` array of a rendered document, from its opening bracket.
    fn output(rendered: &str) -> &str {
        rendered.split("\"output\": ").nth(1).unwrap()
    }

    /// The `output` array of a rendered document, parsed.
    fn output_value(rendered: &str) -> serde_json::Value {
        let document: serde_json::Value = serde_json::from_str(rendered).unwrap();
        document["output"].clone()
    }

    #[test]
    fn renders_the_header_and_an_empty_output() {
        let (document, start) = document();
        let rendered = document
            .render_at(
                "18.5.1.1",
                &connection(),
                0,
                RunEnd::Finished,
                after(start, 42),
            )
            .unwrap();
        assert_eq!(
            rendered,
            format!(
                "{{\n  \"contractVersion\": \"1.0\",\n  \"sqlcmd\": {{\n    \"version\": \"18.5.1.1\",\n    \
                 \"platform\": \"{PLATFORM}\"\n  }},\n  \"connection\": {{\n    \
                 \"server\": \"localhost\",\n    \"database\": \"master\",\n    \
                 \"authentication\": \"SqlPassword\",\n    \"encrypt\": true\n  }},\n  \
                 \"startTime\": \"2026-10-03T02:40:11.483Z\",\n  \"durationMs\": 42,\n  \
                 \"executionStatus\": \"completed\",\n  \"operationOutcome\": \"succeeded\",\n  \
                 \"exitCode\": 0,\n  \"output\": []\n}}\n"
            )
        );
    }

    #[test]
    fn the_connection_adds_server_version_and_connect_time() {
        let (mut document, start) = document();
        document.connecting_at(after(start, 5));
        document.connected_at(Some("17.00.1000".to_string()), after(start, 17));
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 20))
            .unwrap();
        assert!(
            rendered.contains(
                "\"encrypt\": true,\n    \"serverVersion\": \"17.00.1000\",\n    \"connectMs\": 12\n  },"
            ),
            "{rendered}"
        );
    }

    /// Without a `connecting` call, the connection time runs from the start.
    #[test]
    fn connect_time_without_connecting_runs_from_the_start() {
        let (mut document, start) = document();
        document.connected_at(None, after(start, 9));
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 9))
            .unwrap();
        assert!(rendered.contains("\"connectMs\": 9"), "{rendered}");
        assert!(!rendered.contains("serverVersion"), "{rendered}");
    }

    /// A reconnect that fails keeps the live connection and what it reported;
    /// one that succeeds reports itself.
    #[test]
    fn only_a_successful_reconnect_replaces_the_connection() {
        let (mut document, start) = document();
        document.connected_at(Some("16.00".to_string()), after(start, 5));
        document.connecting_at(after(start, 10));
        let rendered = document
            .render_at("v", &connection(), 1, RunEnd::Finished, after(start, 20))
            .unwrap();
        assert!(
            rendered.contains("\"serverVersion\": \"16.00\",\n    \"connectMs\": 5\n"),
            "{rendered}"
        );

        document.connected_at(Some("17.00".to_string()), after(start, 13));
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 20))
            .unwrap();
        assert!(
            rendered.contains("\"serverVersion\": \"17.00\",\n    \"connectMs\": 3\n"),
            "{rendered}"
        );
    }

    /// A non-zero exit code fails the operation; the document names no failure
    /// category, since each error already carries its number and SQLSTATE.
    #[test]
    fn a_non_zero_exit_code_fails_the_operation_without_a_category() {
        let (document, start) = document();
        let rendered = document
            .render_at("v", &connection(), 1, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert!(
            rendered.contains(
                "\"executionStatus\": \"completed\",\n  \"operationOutcome\": \"failed\",\n  \
                 \"exitCode\": 1,"
            ),
            "{rendered}"
        );
        assert!(!rendered.contains("failure"), "{rendered}");
        assert!(!rendered.contains("kind"), "{rendered}");
    }

    /// A canceled run is canceled even when its exit code is 0.
    #[test]
    fn a_canceled_run_is_canceled_whatever_its_exit_code() {
        let (document, start) = document();
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Canceled, after(start, 1))
            .unwrap();
        assert!(
            rendered.contains(
                "\"executionStatus\": \"canceled\",\n  \"operationOutcome\": \"canceled\",\n  \
                 \"exitCode\": 0,"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn a_rejected_command_line_is_an_invalid_invocation() {
        let (document, start) = document();
        let rendered = document
            .render_at(
                "v",
                &Connection::default(),
                1,
                RunEnd::InvalidInvocation,
                after(start, 1),
            )
            .unwrap();
        assert!(
            rendered.contains(
                "\"executionStatus\": \"invalidInvocation\",\n  \"operationOutcome\": \"notExecuted\""
            ),
            "{rendered}"
        );
    }

    #[test]
    fn every_run_end_has_its_status_and_outcome() {
        let cases = [
            (RunEnd::Finished, 0, "completed", "succeeded"),
            (RunEnd::Finished, 1, "completed", "failed"),
            (RunEnd::Finished, -100, "completed", "failed"),
            (RunEnd::Canceled, 0, "canceled", "canceled"),
            (RunEnd::Canceled, 1, "canceled", "canceled"),
            (
                RunEnd::InvalidInvocation,
                1,
                "invalidInvocation",
                "notExecuted",
            ),
        ];
        for (end, exit_code, status, outcome) in cases {
            assert_eq!(
                status_and_outcome(end, exit_code),
                (status, outcome),
                "{end:?} {exit_code}"
            );
        }
    }
    #[test]
    fn renders_batches_result_sets_counts_and_messages_in_order() {
        let (mut document, start) = document();
        document.begin_batch_at(Some("SELECT id FROM t".to_string()), after(start, 1));
        document
            .begin_result_set(vec![column("id"), column("name")])
            .unwrap();
        document
            .add_row(vec![Some("1".to_string()), Some("a".to_string())])
            .unwrap();
        document.add_row(vec![Some("2".to_string()), None]).unwrap();
        document.end_result_set(Some(2)).unwrap();
        document.end_batch_at(after(start, 5));
        document.begin_batch_at(None, after(start, 6));
        document.add_rows_affected(3);
        document.add_message(Message {
            line: Some(2),
            procedure: Some("p".to_string()),
            ..message(true, "boom")
        });
        document.end_batch_at(after(start, 13));

        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 20))
            .unwrap();
        assert_eq!(
            output_value(&rendered),
            serde_json::json!([
                { "type": "batch", "index": 1, "durationMs": 4, "text": "SELECT id FROM t" },
                {
                    "type": "resultSet",
                    "columns": [
                        { "ordinal": 0, "name": "id", "driverType": "int" },
                        { "ordinal": 1, "name": "name", "driverType": "int" }
                    ],
                    "rows": [["1", "a"], ["2", null]],
                    "rowsAffected": 2
                },
                { "type": "batch", "index": 2, "durationMs": 7 },
                { "type": "rowsAffected", "count": 3 },
                {
                    "type": "error", "number": 50000, "severity": 16, "state": 1,
                    "procedure": "p", "line": 2, "text": "boom"
                }
            ])
        );
        // Keys follow the contract's order, not alphabetical order.
        assert!(
            output(&rendered).contains(
                "\"type\": \"error\",\n      \"number\": 50000,\n      \"severity\": 16,\n      \
                 \"state\": 1,\n      \"procedure\": \"p\",\n      \"line\": 2,\n      \
                 \"text\": \"boom\"\n    }"
            ),
            "{rendered}"
        );
    }

    /// Each row and each column description is on one line; everything else
    /// is indented.
    #[test]
    fn rows_and_column_descriptions_are_one_per_line() {
        let (mut document, start) = document();
        document
            .begin_result_set(vec![column("id"), Column::default()])
            .unwrap();
        document.add_row(vec![Some("1".to_string()), None]).unwrap();
        document
            .add_row(vec![Some("2".to_string()), Some("b".to_string())])
            .unwrap();
        document.end_result_set(Some(2)).unwrap();
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert_eq!(
            output(&rendered),
            "[\n    {\n      \"type\": \"resultSet\",\n      \"columns\": [\n        \
             { \"ordinal\": 0, \"name\": \"id\", \"driverType\": \"int\" },\n        \
             { \"ordinal\": 1, \"name\": null }\n      ],\n      \"rows\": [\n        \
             [\"1\", null],\n        [\"2\", \"b\"]\n      ],\n      \"rowsAffected\": 2\n    }\n  ]\n}\n"
        );
    }
    /// A message carries what the console shows (Msg, Level, State, Server,
    /// Procedure, Line, and the driver or sqlcmd when it reported it), then
    /// the SQLSTATE, in that order; anything absent is left out.
    #[test]
    fn messages_carry_the_console_fields_and_the_sqlstate() {
        let (mut document, start) = document();
        document.add_message(Message {
            is_error: true,
            number: 18456,
            severity: 14,
            state: 1,
            server: Some("db01".to_string()),
            line: Some(1),
            sql_state: Some("28000".to_string()),
            text: "Login failed for user 'sa'.".to_string(),
            ..Message::default()
        });
        document.add_message(Message {
            is_error: true,
            source: Some("Microsoft ODBC Driver 18 for SQL Server".to_string()),
            sql_state: Some("08001".to_string()),
            text: "TCP Provider: No connection could be made.".to_string(),
            ..Message::default()
        });
        document.add_message(Message {
            is_error: false,
            number: 0,
            severity: 0,
            state: 1,
            server: Some("db01".to_string()),
            sql_state: Some("01000".to_string()),
            text: "hello".to_string(),
            ..Message::default()
        });
        let rendered = document
            .render_at("v", &connection(), 1, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert_eq!(
            output(&rendered),
            "[\n    {\n      \"type\": \"error\",\n      \"number\": 18456,\n      \"severity\": 14,\n      \
             \"state\": 1,\n      \"server\": \"db01\",\n      \"line\": 1,\n      \
             \"sqlState\": \"28000\",\n      \"text\": \"Login failed for user 'sa'.\"\n    },\n    \
             {\n      \"type\": \"error\",\n      \"number\": 0,\n      \"severity\": 0,\n      \
             \"state\": 0,\n      \"source\": \"Microsoft ODBC Driver 18 for SQL Server\",\n      \
             \"sqlState\": \"08001\",\n      \"text\": \"TCP Provider: No connection could be made.\"\n    },\n    \
             {\n      \"type\": \"message\",\n      \"number\": 0,\n      \"severity\": 0,\n      \
             \"state\": 1,\n      \"server\": \"db01\",\n      \"sqlState\": \"01000\",\n      \
             \"text\": \"hello\"\n    }\n  ]\n}\n"
        );
    }

    /// A batch still running when the document is rendered (the run stopped
    /// inside it) is timed up to the render.
    #[test]
    fn a_running_batch_is_timed_to_the_render() {
        let (mut document, start) = document();
        document.begin_batch_at(None, after(start, 10));
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 25))
            .unwrap();
        assert!(
            rendered.contains("\"index\": 1,\n      \"durationMs\": 15\n"),
            "{rendered}"
        );
    }

    /// Starting a batch ends the one before it, and indexes count up from 1.
    #[test]
    fn a_new_batch_ends_the_previous_one() {
        let (mut document, start) = document();
        document.begin_batch_at(None, after(start, 0));
        document.begin_batch_at(None, after(start, 8));
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 9))
            .unwrap();
        assert!(
            rendered.contains("\"index\": 1,\n      \"durationMs\": 8\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("\"index\": 2,\n      \"durationMs\": 1\n"),
            "{rendered}"
        );
    }

    #[test]
    fn ending_no_batch_does_nothing() {
        let (mut document, start) = document();
        document.end_batch_at(after(start, 1));
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert!(rendered.ends_with("\"output\": []\n}\n"), "{rendered}");
    }

    /// Under `SET NOCOUNT ON` a result set has no count.
    #[test]
    fn a_result_set_without_a_count_has_no_rows_affected() {
        let (mut document, start) = document();
        document.begin_result_set(vec![column("v")]).unwrap();
        document.end_result_set(None).unwrap();
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert!(!rendered.contains("rowsAffected"), "{rendered}");
        assert!(rendered.contains("\"rows\": []\n    }"), "{rendered}");
    }

    /// A result set must be ended before the next one starts; otherwise the
    /// first could never get its count.
    #[test]
    fn a_result_set_cannot_start_while_one_is_open() {
        let (mut document, _) = document();
        document.begin_result_set(vec![column("a")]).unwrap();
        assert_eq!(
            document.begin_result_set(vec![column("b")]),
            Err(FormatError::ResultSetOpen)
        );
        // The rejected call closed the open set: rows that follow are not put
        // in it, and the next result set can start.
        assert_eq!(
            document.add_row(vec![Some("1".to_string())]),
            Err(FormatError::NoResultSet)
        );
        assert_eq!(
            document.end_result_set(Some(1)),
            Err(FormatError::NoResultSet)
        );
        document.begin_result_set(vec![column("b")]).unwrap();
    }

    /// A result set left open when the next batch starts keeps its rows but
    /// takes none of the new batch's rows or its count.
    #[test]
    fn a_new_batch_closes_an_open_result_set() {
        let (mut document, start) = document();
        document.begin_batch(Some("first".to_string()));
        document.begin_result_set(vec![column("a")]).unwrap();
        document.add_row(vec![Some("1".to_string())]).unwrap();
        document.begin_batch(Some("second".to_string()));
        assert_eq!(
            document.add_row(vec![Some("2".to_string())]),
            Err(FormatError::NoResultSet)
        );
        assert_eq!(
            document.end_result_set(Some(99)),
            Err(FormatError::NoResultSet)
        );
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert!(rendered.contains("[\"1\"]"), "{rendered}");
        assert!(!rendered.contains("\"2\""), "{rendered}");
        assert!(!rendered.contains("rowsAffected"), "{rendered}");
    }

    #[test]
    fn ending_a_result_set_closes_it() {
        let (mut document, _) = document();
        document.begin_result_set(vec![column("v")]).unwrap();
        document.end_result_set(Some(0)).unwrap();
        assert_eq!(
            document.add_row(vec![Some("1".to_string())]),
            Err(FormatError::NoResultSet)
        );
        assert_eq!(
            document.end_result_set(Some(0)),
            Err(FormatError::NoResultSet)
        );
    }

    /// SQL `NULL` and the string "NULL" must stay distinguishable.
    #[test]
    fn null_is_json_null_and_the_string_null_is_a_string() {
        let (mut document, start) = document();
        document.begin_result_set(vec![column("v")]).unwrap();
        document.add_row(vec![None]).unwrap();
        document.add_row(vec![Some("NULL".to_string())]).unwrap();
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert_eq!(
            output_value(&rendered)[0]["rows"],
            serde_json::json!([[null], ["NULL"]]),
            "{rendered}"
        );
    }

    #[test]
    fn unknown_connection_fields_are_null() {
        let (document, start) = document();
        let rendered = document
            .render_at(
                "v",
                &Connection::default(),
                0,
                RunEnd::Finished,
                after(start, 1),
            )
            .unwrap();
        assert!(rendered.contains("\"server\": null"));
        assert!(rendered.contains("\"database\": null"));
        assert!(rendered.contains("\"authentication\": null"));
        assert!(rendered.contains("\"encrypt\": false\n  },"));
    }

    /// Quotes, backslashes and control characters are escaped (RFC 8259);
    /// everything else, non-ASCII included, is written as is.
    #[test]
    fn strings_are_escaped() {
        let (mut document, start) = document();
        document.add_message(message(
            false,
            "q\"b\\n\nr\rt\tb\u{08}f\u{0C}c\u{01}é\u{1F600}",
        ));
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert!(
            rendered.contains("\"text\": \"q\\\"b\\\\n\\nr\\rt\\tb\\bf\\fc\\u0001é\u{1F600}\""),
            "{rendered}"
        );
    }

    #[test]
    fn a_row_before_any_result_set_is_rejected() {
        let (mut document, _) = document();
        assert_eq!(
            document.add_row(vec![Some("1".to_string())]),
            Err(FormatError::NoResultSet)
        );
    }

    #[test]
    fn a_row_with_the_wrong_value_count_is_rejected() {
        let (mut document, _) = document();
        document
            .begin_result_set(vec![column("a"), column("b")])
            .unwrap();
        assert_eq!(
            document.add_row(vec![Some("1".to_string())]),
            Err(FormatError::ColumnCountMismatch {
                expected: 2,
                actual: 1
            })
        );
    }

    /// A message can arrive while a result set's rows are still being read;
    /// later rows still belong to that result set.
    #[test]
    fn rows_after_a_message_stay_in_their_result_set() {
        let (mut document, start) = document();
        document.begin_result_set(vec![column("v")]).unwrap();
        document.add_row(vec![Some("1".to_string())]).unwrap();
        document.add_message(message(false, "warning"));
        document.add_row(vec![Some("2".to_string())]).unwrap();
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        let output = output_value(&rendered);
        assert_eq!(
            output[0]["rows"],
            serde_json::json!([["1"], ["2"]]),
            "{rendered}"
        );
        assert_eq!(output[1]["text"], "warning", "{rendered}");
    }

    /// `FOR JSON` output is ordinary rows: one string per row the server sent,
    /// not parsed, combined or embedded, and counted like any result set.
    #[test]
    fn for_json_output_is_ordinary_string_rows() {
        let (mut document, start) = document();
        document
            .begin_result_set(vec![Column {
                name: Some("JSON_F52E2B61-18A1-11d1-B105-00805F49916B".to_string()),
                driver_type: Some("nvarchar".to_string()),
                ..Column::default()
            }])
            .unwrap();
        document
            .add_row(vec![Some("[{\"col1\":1,\"col2\":\"al".to_string())])
            .unwrap();
        document.add_row(vec![Some("pha\"}]".to_string())]).unwrap();
        document.end_result_set(Some(2)).unwrap();
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        let result_set = &output_value(&rendered)[0];
        assert_eq!(
            result_set["rows"],
            serde_json::json!([["[{\"col1\":1,\"col2\":\"al"], ["pha\"}]"]]),
            "{rendered}"
        );
        assert_eq!(result_set["rowsAffected"], 2, "{rendered}");
    }

    /// Columns carry `ordinal` and `name` always (`null` when unnamed), and the
    /// rest only when the driver exposed it, unchanged.
    #[test]
    fn columns_are_described_as_the_driver_describes_them() {
        let (mut document, start) = document();
        document
            .begin_result_set(vec![
                Column {
                    name: Some("value".to_string()),
                    driver_type: Some("nvarchar".to_string()),
                    size: Some(20),
                    nullable: Some(false),
                    ..Column::default()
                },
                Column {
                    name: Some("value".to_string()),
                    driver_type: Some("int identity".to_string()),
                    precision: Some(10),
                    scale: Some(0),
                    nullable: Some(true),
                    ..Column::default()
                },
                Column::default(),
            ])
            .unwrap();
        let rendered = document
            .render_at("v", &connection(), 0, RunEnd::Finished, after(start, 1))
            .unwrap();
        assert_eq!(
            output_value(&rendered)[0]["columns"],
            serde_json::json!([
                { "ordinal": 0, "name": "value", "driverType": "nvarchar", "size": 20, "nullable": false },
                {
                    "ordinal": 1, "name": "value", "driverType": "int identity",
                    "precision": 10, "scale": 0, "nullable": true
                },
                { "ordinal": 2, "name": null }
            ]),
            "{rendered}"
        );
        // Absent metadata is left out, not written as null.
        assert!(!rendered.contains("\"size\": null"), "{rendered}");
    }
    #[test]
    fn timestamps_are_utc_with_milliseconds() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_timestamp(START_UNIX_MS), "2026-10-03T02:40:11.483Z");
        // Leap day, and the last millisecond of a century leap year.
        assert_eq!(utc_timestamp(1_709_164_800_007), "2024-02-29T00:00:00.007Z");
        assert_eq!(utc_timestamp(978_307_199_999), "2000-12-31T23:59:59.999Z");
    }

    #[test]
    fn the_platform_is_the_targets_runtime_identifier() {
        // Spelled out per target, independently of platform(), so a wrong
        // mapping that is still a valid identifier fails here. Targets outside
        // the supported matrix get their operating system's placeholder.
        let expected = if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            "win-x64"
        } else if cfg!(all(target_os = "windows", target_arch = "x86")) {
            "win-x86"
        } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
            "win-arm64"
        } else if cfg!(all(
            target_os = "linux",
            target_arch = "x86_64",
            target_env = "musl"
        )) {
            "linux-musl-x64"
        } else if cfg!(all(
            target_os = "linux",
            target_arch = "aarch64",
            target_env = "musl"
        )) {
            "linux-musl-arm64"
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            "linux-x64"
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            "linux-arm64"
        } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            "osx-x64"
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            "osx-arm64"
        } else if cfg!(target_os = "windows") {
            "win"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "osx"
        } else {
            "unknown"
        };
        assert_eq!(PLATFORM, expected);
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! JSON output for `--format json`.
//!
//! Native sqlcmd runs the batches as usual and, instead of printing text, hands
//! each piece of output to a [`JsonDocument`]: the batches it sends, result
//! sets and their rows, row counts, and server messages. When sqlcmd exits it
//! calls [`JsonDocument::render`] with the connection details, the exit code
//! and, for a failed run, why it failed, and prints the one JSON document that
//! returns:
//!
//! ```json
//! {
//!   "formatVersion": 1,
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
//!   "status": "success",
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
//!         { "name": "id", "type": "int" },
//!         { "name": "name", "type": "nvarchar(50)" }
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
//! Values are strings, exactly as sqlcmd would print them, so no precision is
//! lost and every SQL type has a representation. SQL `NULL` is JSON `null`,
//! which keeps it distinct from the string `"NULL"`.
//!
//! A result set produced by `FOR JSON` or `FOR XML` keeps the server's own
//! conversion: its rows, which the server splits into chunks, are joined back
//! into one value. JSON is written into the document exactly as the server sent
//! it, once it is known to parse; text that does not parse (for example
//! `WITHOUT_ARRAY_WRAPPER` over several rows) and XML are written as strings.
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

use std::fmt::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Version of the document's shape. It changes only when a change could break
/// a consumer, such as a field renamed or removed.
pub const FORMAT_VERSION: u32 = 1;

/// The column name SQL Server gives the single column of a `FOR JSON` result.
const FOR_JSON_COLUMN: &str = "JSON_F52E2B61-18A1-11d1-B105-00805F49916B";
/// The column name SQL Server gives the single column of a `FOR XML` result.
const FOR_XML_COLUMN: &str = "XML_F52E2B61-18A1-11d1-B105-00805F49916B";

/// Deepest nesting accepted when checking that `FOR JSON` output parses. SQL
/// Server nests `FOR JSON` output at most 128 levels deep.
const MAX_JSON_DEPTH: usize = 512;

/// The runtime this library was built for, as a NuGet runtime identifier.
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
        } else {
            "osx-x64"
        }
    } else if cfg!(target_os = "linux") {
        let musl = cfg!(target_env = "musl");
        if cfg!(target_arch = "aarch64") {
            if musl {
                "linux-musl-arm64"
            } else {
                "linux-arm64"
            }
        } else if musl {
            "linux-musl-x64"
        } else {
            "linux-x64"
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
    /// The database named on the command line; `null` when none was named,
    /// since the login's default database is not known to sqlcmd.
    pub database: Option<String>,
    /// How sqlcmd authenticated, e.g. `Integrated` or `SqlPassword`.
    pub authentication: Option<String>,
    /// Whether the connection was encrypted.
    pub encrypt: bool,
}

/// Why a run failed, rendered as `failure.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The server could not be reached, or the connection was lost.
    Connection,
    /// The server refused the login.
    Authentication,
    /// A statement error stopped the run (`-b`, `-V`, or `RAISERROR` with
    /// state 127).
    Query,
    /// The last error before the run failed was a query timeout. (sqlcmd
    /// itself does not stop on a timeout, even under `-b`.)
    Timeout,
    /// The run was cancelled, e.g. with Ctrl+C.
    Cancelled,
    /// Anything else, such as an input file that could not be read or an exit
    /// code chosen with `:EXIT`.
    Other,
}

impl Failure {
    fn kind(self) -> &'static str {
        match self {
            Failure::Connection => "connection",
            Failure::Authentication => "authentication",
            Failure::Query => "query",
            Failure::Timeout => "timeout",
            Failure::Cancelled => "cancelled",
            Failure::Other => "other",
        }
    }
}

/// A result-set column: its name and its SQL type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// The SQL type as written in T-SQL, e.g. `int` or `nvarchar(50)`; see
    /// [`sql_type`].
    pub sql_type: String,
}

/// Writes a column's SQL type the way T-SQL declares it, from the server's
/// type name and the size, precision and scale the driver reports.
///
/// `length` is in characters for character types and in bytes for binary
/// types; 0 or less means `max`. The driver names an identity column's type
/// with an ` identity` suffix (and `decimal() identity` for decimals); the
/// column's type is the base type.
pub fn sql_type(type_name: &str, length: i64, precision: i32, scale: i32) -> String {
    let mut name = type_name.trim();
    let suffix = " identity";
    if name.len() > suffix.len()
        && name.is_char_boundary(name.len() - suffix.len())
        && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    {
        name = name[..name.len() - suffix.len()].trim_end();
    }
    let name = name.strip_suffix("()").unwrap_or(name);
    match name.to_ascii_lowercase().as_str() {
        "char" | "varchar" | "nchar" | "nvarchar" | "binary" | "varbinary" => {
            if length <= 0 || length > 8000 {
                format!("{name}(max)")
            } else {
                format!("{name}({length})")
            }
        }
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        _ => name.to_string(),
    }
}

/// A server message or error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// `true` for an error, `false` for an informational message such as
    /// `PRINT`.
    pub is_error: bool,
    pub number: i32,
    pub state: i32,
    pub severity: i32,
    /// The line the server reported, when it reported one.
    pub line: Option<i32>,
    /// The stored procedure the message came from, when there was one.
    pub procedure: Option<String>,
    pub text: String,
}

/// Why a call could not be applied to the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// A row or count arrived when no result set was open.
    NoResultSet,
    /// A row's value count differs from the result set's column count.
    ColumnCountMismatch { expected: usize, actual: usize },
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::NoResultSet => write!(f, "no result set is open"),
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

/// What a single-column `FOR JSON` or `FOR XML` result renders as.
enum ServerFormatted<'a> {
    Json(Option<String>),
    UnparsedJson(String),
    Xml(Option<String>),
    /// Not a `FOR JSON` or `FOR XML` result.
    No(&'a [Vec<Option<String>>]),
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
    pub fn starting_at(start_unix_ms: u64, start: Instant) -> Self {
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

    /// Notes that sqlcmd started connecting. Whatever an earlier connection
    /// reported is cleared, so a failed reconnect (`:CONNECT`) leaves no
    /// details of a connection that is gone.
    pub fn connecting(&mut self) {
        self.connecting_at(Instant::now());
    }

    pub fn connecting_at(&mut self, now: Instant) {
        self.connecting = Some(now);
        self.server_version = None;
        self.connect_ms = None;
    }

    /// Notes that sqlcmd connected, and the version the server reported.
    /// `connectMs` runs from the matching [`connecting`](Self::connecting),
    /// or from the start if there was none. A later connection (`:CONNECT`)
    /// replaces both.
    pub fn connected(&mut self, server_version: Option<String>) {
        self.connected_at(server_version, Instant::now());
    }

    pub fn connected_at(&mut self, server_version: Option<String>, now: Instant) {
        let from = self.connecting.take().unwrap_or(self.start);
        self.connect_ms = Some(millis_between(from, now));
        self.server_version = server_version;
    }

    /// Starts a batch sent to the server. `text` is the batch as sent, or
    /// `None` to leave it out (sqlcmd includes it only with `-e`). A batch
    /// still running is ended first.
    pub fn begin_batch(&mut self, text: Option<String>) {
        self.begin_batch_at(text, Instant::now());
    }

    pub fn begin_batch_at(&mut self, text: Option<String>, now: Instant) {
        self.end_batch_at(now);
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

    pub fn end_batch_at(&mut self, now: Instant) {
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

    /// Starts a result set; the rows that follow belong to it.
    pub fn begin_result_set(&mut self, columns: Vec<Column>) {
        self.current_result_set = Some(self.output.len());
        self.output.push(Output::ResultSet {
            columns,
            rows: Vec::new(),
            rows_affected: None,
        });
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
    /// The run is `failed` when `failure` is given or the exit code is not 0;
    /// a non-zero exit code with no reason given renders `failure.kind` as
    /// `other`.
    pub fn render(
        &self,
        version: &str,
        connection: &Connection,
        exit_code: i32,
        failure: Option<Failure>,
    ) -> String {
        self.render_at(version, connection, exit_code, failure, Instant::now())
    }

    pub fn render_at(
        &self,
        version: &str,
        connection: &Connection,
        exit_code: i32,
        failure: Option<Failure>,
        now: Instant,
    ) -> String {
        let failure = failure.or((exit_code != 0).then_some(Failure::Other));
        let mut out = String::new();
        let _ = write!(out, "{{\n  \"formatVersion\": {FORMAT_VERSION},");
        out.push_str("\n  \"sqlcmd\": {\n    \"version\": ");
        push_string(&mut out, version);
        out.push_str(",\n    \"platform\": ");
        push_string(&mut out, PLATFORM);
        out.push_str("\n  },\n  \"connection\": {\n    \"server\": ");
        push_optional_string(&mut out, connection.server.as_deref());
        out.push_str(",\n    \"database\": ");
        push_optional_string(&mut out, connection.database.as_deref());
        out.push_str(",\n    \"authentication\": ");
        push_optional_string(&mut out, connection.authentication.as_deref());
        out.push_str(",\n    \"encrypt\": ");
        out.push_str(if connection.encrypt { "true" } else { "false" });
        if let Some(server_version) = &self.server_version {
            out.push_str(",\n    \"serverVersion\": ");
            push_string(&mut out, server_version);
        }
        if let Some(connect_ms) = self.connect_ms {
            let _ = write!(out, ",\n    \"connectMs\": {connect_ms}");
        }
        out.push_str("\n  },\n  \"startTime\": ");
        push_string(&mut out, &utc_timestamp(self.start_unix_ms));
        let _ = write!(
            out,
            ",\n  \"durationMs\": {}",
            millis_between(self.start, now)
        );
        out.push_str(",\n  \"status\": ");
        push_string(
            &mut out,
            if failure.is_some() {
                "failed"
            } else {
                "success"
            },
        );
        if let Some(failure) = failure {
            out.push_str(",\n  \"failure\": {\n    \"kind\": ");
            push_string(&mut out, failure.kind());
            out.push_str("\n  }");
        }
        let _ = write!(out, ",\n  \"exitCode\": {exit_code},\n  \"output\": [");
        for (index, item) in self.output.iter().enumerate() {
            out.push_str(if index == 0 { "\n" } else { ",\n" });
            let running_batch = self.current_batch == Some(index);
            push_output(&mut out, item, running_batch.then_some(now));
        }
        out.push_str(if self.output.is_empty() {
            "]\n}\n"
        } else {
            "\n  ]\n}\n"
        });
        out
    }
}

/// Writes one `output` entry. `now` is given for the batch still running, whose
/// duration runs to the time of rendering.
fn push_output(out: &mut String, item: &Output, now: Option<Instant>) {
    match item {
        Output::Batch {
            index,
            text,
            started,
            duration_ms,
        } => {
            let _ = write!(
                out,
                "    {{\n      \"type\": \"batch\",\n      \"index\": {index}"
            );
            let duration = duration_ms.or_else(|| now.map(|now| millis_between(*started, now)));
            if let Some(duration) = duration {
                let _ = write!(out, ",\n      \"durationMs\": {duration}");
            }
            if let Some(text) = text {
                out.push_str(",\n      \"text\": ");
                push_string(out, text);
            }
            out.push_str("\n    }");
        }
        Output::ResultSet {
            columns,
            rows,
            rows_affected,
        } => {
            out.push_str("    {\n      \"type\": \"resultSet\",\n      \"columns\": [");
            for (index, column) in columns.iter().enumerate() {
                out.push_str(if index == 0 {
                    "\n        { \"name\": "
                } else {
                    ",\n        { \"name\": "
                });
                push_string(out, &column.name);
                out.push_str(", \"type\": ");
                push_string(out, &column.sql_type);
                out.push_str(" }");
            }
            out.push_str(if columns.is_empty() { "]" } else { "\n      ]" });
            match server_formatted(columns, rows) {
                ServerFormatted::Json(json) => {
                    out.push_str(",\n      \"serverFormat\": \"json\",\n      \"json\": ");
                    // Already checked to parse: written as the server sent it.
                    out.push_str(json.as_deref().unwrap_or("null"));
                }
                ServerFormatted::UnparsedJson(text) => {
                    out.push_str(",\n      \"serverFormat\": \"json\",\n      \"text\": ");
                    push_string(out, &text);
                }
                ServerFormatted::Xml(xml) => {
                    out.push_str(",\n      \"serverFormat\": \"xml\",\n      \"xml\": ");
                    push_optional_string(out, xml.as_deref());
                }
                ServerFormatted::No(rows) => {
                    out.push_str(",\n      \"rows\": [");
                    for (index, row) in rows.iter().enumerate() {
                        out.push_str(if index == 0 {
                            "\n        "
                        } else {
                            ",\n        "
                        });
                        push_array(out, row.iter().map(Option::as_deref));
                    }
                    out.push_str(if rows.is_empty() { "]" } else { "\n      ]" });
                    // The count of a server-formatted result is its chunks,
                    // which mean nothing to a reader, so it is shown only here.
                    if let Some(count) = rows_affected {
                        let _ = write!(out, ",\n      \"rowsAffected\": {count}");
                    }
                }
            }
            out.push_str("\n    }");
        }
        Output::RowsAffected(count) => {
            out.push_str("    {\n      \"type\": \"rowsAffected\",\n      \"count\": ");
            let _ = write!(out, "{count}");
            out.push_str("\n    }");
        }
        Output::Message(message) => {
            out.push_str("    {\n      \"type\": ");
            push_string(out, if message.is_error { "error" } else { "message" });
            let _ = write!(
                out,
                ",\n      \"number\": {},\n      \"state\": {},\n      \"severity\": {}",
                message.number, message.state, message.severity
            );
            if let Some(line) = message.line {
                let _ = write!(out, ",\n      \"line\": {line}");
            }
            if let Some(procedure) = &message.procedure {
                out.push_str(",\n      \"procedure\": ");
                push_string(out, procedure);
            }
            out.push_str(",\n      \"message\": ");
            push_string(out, &message.text);
            out.push_str("\n    }");
        }
    }
}

/// Recognizes a `FOR JSON` or `FOR XML` result by its single, well-known
/// column, and joins the chunks the server split it into. A result with no
/// rows, or only `NULL`s, has no value.
fn server_formatted<'a>(
    columns: &[Column],
    rows: &'a [Vec<Option<String>>],
) -> ServerFormatted<'a> {
    let [column] = columns else {
        return ServerFormatted::No(rows);
    };
    let is_json = column.name == FOR_JSON_COLUMN;
    if !is_json && column.name != FOR_XML_COLUMN {
        return ServerFormatted::No(rows);
    }
    let mut joined: Option<String> = None;
    for value in rows.iter().filter_map(|row| row.first()?.as_deref()) {
        joined.get_or_insert_with(String::new).push_str(value);
    }
    match (is_json, joined) {
        (true, Some(text)) if !is_valid_json(&text) => ServerFormatted::UnparsedJson(text),
        (true, joined) => ServerFormatted::Json(joined),
        (false, joined) => ServerFormatted::Xml(joined),
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

/// Whether `text` is one JSON value (RFC 8259), optionally surrounded by
/// whitespace.
fn is_valid_json(text: &str) -> bool {
    let mut parser = JsonChecker {
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_whitespace();
    if !parser.value(0) {
        return false;
    }
    parser.skip_whitespace();
    parser.pos == parser.bytes.len()
}

/// A syntax-only JSON checker: it builds nothing, so checking costs no memory
/// beyond the input.
struct JsonChecker<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl JsonChecker<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self, depth: usize) -> bool {
        if depth > MAX_JSON_DEPTH {
            return false;
        }
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => self.string(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            _ => false,
        }
    }

    fn object(&mut self, depth: usize) -> bool {
        self.pos += 1;
        self.skip_whitespace();
        if self.eat(b'}') {
            return true;
        }
        loop {
            self.skip_whitespace();
            if !self.string() {
                return false;
            }
            self.skip_whitespace();
            if !self.eat(b':') {
                return false;
            }
            self.skip_whitespace();
            if !self.value(depth) {
                return false;
            }
            self.skip_whitespace();
            if self.eat(b'}') {
                return true;
            }
            if !self.eat(b',') {
                return false;
            }
        }
    }

    fn array(&mut self, depth: usize) -> bool {
        self.pos += 1;
        self.skip_whitespace();
        if self.eat(b']') {
            return true;
        }
        loop {
            self.skip_whitespace();
            if !self.value(depth) {
                return false;
            }
            self.skip_whitespace();
            if self.eat(b']') {
                return true;
            }
            if !self.eat(b',') {
                return false;
            }
        }
    }

    fn string(&mut self) -> bool {
        if !self.eat(b'"') {
            return false;
        }
        while let Some(byte) = self.peek() {
            self.pos += 1;
            match byte {
                b'"' => return true,
                b'\\' => match self.peek() {
                    Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.pos += 1,
                    Some(b'u') => {
                        self.pos += 1;
                        for _ in 0..4 {
                            if !matches!(self.peek(), Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
                            {
                                return false;
                            }
                            self.pos += 1;
                        }
                    }
                    _ => return false,
                },
                0x00..=0x1f => return false,
                _ => {}
            }
        }
        false
    }

    fn number(&mut self) -> bool {
        self.eat(b'-');
        if !self.eat(b'0') {
            if !matches!(self.peek(), Some(b'1'..=b'9')) {
                return false;
            }
            self.digits();
        }
        if self.eat(b'.') && !self.digits() {
            return false;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if !self.digits() {
                return false;
            }
        }
        true
    }

    /// Consumes one or more digits; `false` if there were none.
    fn digits(&mut self) -> bool {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos > start
    }

    fn literal(&mut self, word: &[u8]) -> bool {
        if self.bytes[self.pos..].starts_with(word) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }
}

fn push_array<'a>(out: &mut String, values: impl Iterator<Item = Option<&'a str>>) {
    out.push('[');
    for (index, value) in values.enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        push_optional_string(out, value);
    }
    out.push(']');
}

fn push_optional_string(out: &mut String, value: Option<&str>) {
    match value {
        Some(value) => push_string(out, value),
        None => out.push_str("null"),
    }
}

/// Writes `value` as a JSON string (RFC 8259): quotes, backslashes and control
/// characters are escaped, everything else is written as is.
fn push_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if c < '\u{20}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
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
            name: name.to_string(),
            sql_type: "int".to_string(),
        }
    }

    fn message(is_error: bool, text: &str) -> Message {
        Message {
            is_error,
            number: if is_error { 50000 } else { 0 },
            state: 1,
            severity: if is_error { 16 } else { 0 },
            line: None,
            procedure: None,
            text: text.to_string(),
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

    #[test]
    fn renders_the_header_and_an_empty_output() {
        let (document, start) = document();
        let rendered = document.render_at("18.5.1.1", &connection(), 0, None, after(start, 42));
        assert_eq!(
            rendered,
            format!(
                "{{\n  \"formatVersion\": 1,\n  \"sqlcmd\": {{\n    \"version\": \"18.5.1.1\",\n    \
                 \"platform\": \"{PLATFORM}\"\n  }},\n  \"connection\": {{\n    \
                 \"server\": \"localhost\",\n    \"database\": \"master\",\n    \
                 \"authentication\": \"SqlPassword\",\n    \"encrypt\": true\n  }},\n  \
                 \"startTime\": \"2026-10-03T02:40:11.483Z\",\n  \"durationMs\": 42,\n  \
                 \"status\": \"success\",\n  \"exitCode\": 0,\n  \"output\": []\n}}\n"
            )
        );
    }

    #[test]
    fn the_connection_adds_server_version_and_connect_time() {
        let (mut document, start) = document();
        document.connecting_at(after(start, 5));
        document.connected_at(Some("17.00.1000".to_string()), after(start, 17));
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 20));
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
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 9));
        assert!(rendered.contains("\"connectMs\": 9"), "{rendered}");
        assert!(!rendered.contains("serverVersion"), "{rendered}");
    }

    /// A reconnect that fails leaves nothing of the connection before it; one
    /// that succeeds reports itself.
    #[test]
    fn a_new_connection_attempt_clears_the_previous_one() {
        let (mut document, start) = document();
        document.connected_at(Some("16.00".to_string()), after(start, 5));
        document.connecting_at(after(start, 10));
        let rendered = document.render_at("v", &connection(), 1, None, after(start, 20));
        assert!(!rendered.contains("serverVersion"), "{rendered}");
        assert!(!rendered.contains("connectMs"), "{rendered}");

        document.connected_at(Some("17.00".to_string()), after(start, 13));
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 20));
        assert!(
            rendered.contains("\"serverVersion\": \"17.00\",\n    \"connectMs\": 3\n"),
            "{rendered}"
        );
    }

    #[test]
    fn a_failure_is_rendered_with_its_kind() {
        let (document, start) = document();
        let rendered = document.render_at(
            "v",
            &connection(),
            1,
            Some(Failure::Connection),
            after(start, 1),
        );
        assert!(
            rendered.contains(
                "\"status\": \"failed\",\n  \"failure\": {\n    \"kind\": \"connection\"\n  },\n  \"exitCode\": 1,"
            ),
            "{rendered}"
        );
    }

    /// A run stopped with a non-zero exit code and no reason given still
    /// reads as failed, so `status` always agrees with the exit code.
    #[test]
    fn a_non_zero_exit_code_alone_is_a_failure_of_kind_other() {
        let (document, start) = document();
        let rendered = document.render_at("v", &connection(), 7, None, after(start, 1));
        assert!(rendered.contains("\"status\": \"failed\""), "{rendered}");
        assert!(rendered.contains("\"kind\": \"other\""), "{rendered}");
    }

    /// A cancelled run is failed even when its exit code is 0.
    #[test]
    fn a_failure_with_exit_code_zero_is_still_failed() {
        let (document, start) = document();
        let rendered = document.render_at(
            "v",
            &connection(),
            0,
            Some(Failure::Cancelled),
            after(start, 1),
        );
        assert!(rendered.contains("\"status\": \"failed\""), "{rendered}");
        assert!(rendered.contains("\"kind\": \"cancelled\""), "{rendered}");
    }

    #[test]
    fn every_failure_kind_has_its_name() {
        let kinds = [
            (Failure::Connection, "connection"),
            (Failure::Authentication, "authentication"),
            (Failure::Query, "query"),
            (Failure::Timeout, "timeout"),
            (Failure::Cancelled, "cancelled"),
            (Failure::Other, "other"),
        ];
        for (failure, kind) in kinds {
            assert_eq!(failure.kind(), kind);
        }
    }

    #[test]
    fn renders_batches_result_sets_counts_and_messages_in_order() {
        let (mut document, start) = document();
        document.begin_batch_at(Some("SELECT id FROM t".to_string()), after(start, 1));
        document.begin_result_set(vec![column("id"), column("name")]);
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

        let rendered = document.render_at("v", &connection(), 0, None, after(start, 20));
        assert_eq!(
            output(&rendered),
            "[\n    {\n      \"type\": \"batch\",\n      \"index\": 1,\n      \"durationMs\": 4,\n      \
             \"text\": \"SELECT id FROM t\"\n    },\n    {\n      \"type\": \"resultSet\",\n      \
             \"columns\": [\n        { \"name\": \"id\", \"type\": \"int\" },\n        \
             { \"name\": \"name\", \"type\": \"int\" }\n      ],\n      \"rows\": [\n        \
             [\"1\", \"a\"],\n        [\"2\", null]\n      ],\n      \"rowsAffected\": 2\n    },\n    \
             {\n      \"type\": \"batch\",\n      \"index\": 2,\n      \"durationMs\": 7\n    },\n    \
             {\n      \"type\": \"rowsAffected\",\n      \"count\": 3\n    },\n    \
             {\n      \"type\": \"error\",\n      \"number\": 50000,\n      \"state\": 1,\n      \
             \"severity\": 16,\n      \"line\": 2,\n      \"procedure\": \"p\",\n      \
             \"message\": \"boom\"\n    }\n  ]\n}\n"
        );
    }

    /// A batch still running when the document is rendered (the run stopped
    /// inside it) is timed up to the render.
    #[test]
    fn a_running_batch_is_timed_to_the_render() {
        let (mut document, start) = document();
        document.begin_batch_at(None, after(start, 10));
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 25));
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
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 9));
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
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 1));
        assert!(rendered.ends_with("\"output\": []\n}\n"), "{rendered}");
    }

    /// Under `SET NOCOUNT ON` a result set has no count.
    #[test]
    fn a_result_set_without_a_count_has_no_rows_affected() {
        let (mut document, start) = document();
        document.begin_result_set(vec![column("v")]);
        document.end_result_set(None).unwrap();
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 1));
        assert!(!rendered.contains("rowsAffected"), "{rendered}");
        assert!(rendered.contains("\"rows\": []\n    }"), "{rendered}");
    }

    #[test]
    fn ending_a_result_set_closes_it() {
        let (mut document, _) = document();
        document.begin_result_set(vec![column("v")]);
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
        document.begin_result_set(vec![column("v")]);
        document.add_row(vec![None]).unwrap();
        document.add_row(vec![Some("NULL".to_string())]).unwrap();
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 1));
        assert!(
            rendered.contains("[null],\n        [\"NULL\"]"),
            "{rendered}"
        );
    }

    #[test]
    fn unknown_connection_fields_are_null() {
        let (document, start) = document();
        let rendered = document.render_at("v", &Connection::default(), 0, None, after(start, 1));
        assert!(rendered.contains("\"server\": null"));
        assert!(rendered.contains("\"database\": null"));
        assert!(rendered.contains("\"authentication\": null"));
        assert!(rendered.contains("\"encrypt\": false\n  },"));
    }

    #[test]
    fn strings_are_escaped() {
        let mut out = String::new();
        push_string(&mut out, "q\"b\\n\nr\rt\tb\u{08}f\u{0C}c\u{01}é\u{1F600}");
        assert_eq!(out, "\"q\\\"b\\\\n\\nr\\rt\\tb\\bf\\fc\\u0001é\u{1F600}\"");
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
        document.begin_result_set(vec![column("a"), column("b")]);
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
        document.begin_result_set(vec![column("v")]);
        document.add_row(vec![Some("1".to_string())]).unwrap();
        document.add_message(message(false, "warning"));
        document.add_row(vec![Some("2".to_string())]).unwrap();
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 1));
        assert!(rendered.contains("[\"1\"],\n        [\"2\"]"), "{rendered}");
    }

    fn for_json(chunks: &[Option<&str>]) -> String {
        let (mut document, start) = document();
        document.begin_result_set(vec![Column {
            name: FOR_JSON_COLUMN.to_string(),
            sql_type: "nvarchar(max)".to_string(),
        }]);
        for chunk in chunks {
            document.add_row(vec![chunk.map(str::to_string)]).unwrap();
        }
        document
            .end_result_set(Some(i64::try_from(chunks.len()).unwrap()))
            .unwrap();
        document.render_at("v", &connection(), 0, None, after(start, 1))
    }

    /// The chunks are joined and the server's JSON is written as is, with no
    /// count (it would count chunks).
    #[test]
    fn for_json_output_is_joined_and_embedded_verbatim() {
        let rendered = for_json(&[
            Some("[{\"col1\":1,\"col2\":\"al"),
            Some("pha\"},{\"x\":1.50E+2}]"),
        ]);
        assert!(
            rendered.contains(
                "\"serverFormat\": \"json\",\n      \"json\": [{\"col1\":1,\"col2\":\"alpha\"},{\"x\":1.50E+2}]\n    }"
            ),
            "{rendered}"
        );
        assert!(!rendered.contains("rowsAffected"), "{rendered}");
        assert!(!rendered.contains("\"rows\""), "{rendered}");
    }

    /// `WITHOUT_ARRAY_WRAPPER` over several rows is not one JSON value; the
    /// server's text is kept as a string instead.
    #[test]
    fn for_json_output_that_does_not_parse_is_kept_as_text() {
        let rendered = for_json(&[Some("{\"a\":1},{\"a\":2}")]);
        assert!(
            rendered.contains(
                "\"serverFormat\": \"json\",\n      \"text\": \"{\\\"a\\\":1},{\\\"a\\\":2}\"\n    }"
            ),
            "{rendered}"
        );
    }

    /// `FOR JSON` over no rows returns no rows, so there is no value.
    #[test]
    fn for_json_over_no_rows_is_null() {
        let rendered = for_json(&[]);
        assert!(rendered.contains("\"json\": null\n    }"), "{rendered}");
    }

    #[test]
    fn for_xml_output_is_joined_into_a_string() {
        let (mut document, start) = document();
        document.begin_result_set(vec![Column {
            name: FOR_XML_COLUMN.to_string(),
            sql_type: "ntext".to_string(),
        }]);
        document.add_row(vec![Some("<r><x>1".to_string())]).unwrap();
        document
            .add_row(vec![Some("</x></r>".to_string())])
            .unwrap();
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 1));
        assert!(
            rendered
                .contains("\"serverFormat\": \"xml\",\n      \"xml\": \"<r><x>1</x></r>\"\n    }"),
            "{rendered}"
        );
    }

    /// Only the server's own column name marks server-formatted output: the
    /// same JSON under a column the query named is an ordinary value.
    #[test]
    fn json_in_an_ordinary_column_is_an_ordinary_value() {
        let (mut document, start) = document();
        document.begin_result_set(vec![column("doc")]);
        document.add_row(vec![Some("[1]".to_string())]).unwrap();
        let rendered = document.render_at("v", &connection(), 0, None, after(start, 1));
        assert!(
            rendered.contains("\"rows\": [\n        [\"[1]\"]"),
            "{rendered}"
        );
        assert!(!rendered.contains("serverFormat"), "{rendered}");
    }

    #[test]
    fn sql_types_are_written_as_t_sql_declares_them() {
        assert_eq!(sql_type("int", 4, 10, 0), "int");
        assert_eq!(sql_type("nvarchar", 50, 0, 0), "nvarchar(50)");
        assert_eq!(sql_type("nvarchar", 0, 0, 0), "nvarchar(max)");
        assert_eq!(sql_type("varbinary", 8001, 0, 0), "varbinary(max)");
        assert_eq!(sql_type("char", 8000, 0, 0), "char(8000)");
        assert_eq!(sql_type("decimal", 5, 10, 2), "decimal(10,2)");
        assert_eq!(sql_type("numeric", 20, 18, 0), "numeric(18,0)");
        assert_eq!(sql_type("datetime2", 27, 27, 7), "datetime2(7)");
        assert_eq!(sql_type("time", 16, 16, 3), "time(3)");
        assert_eq!(sql_type("datetimeoffset", 34, 34, 7), "datetimeoffset(7)");
        assert_eq!(sql_type("xml", 0, 0, 0), "xml");
        assert_eq!(sql_type(" uniqueidentifier ", 36, 0, 0), "uniqueidentifier");
    }

    /// The driver reports an identity column's type as `int identity` or
    /// `decimal() identity`; the column's type is the base type.
    #[test]
    fn identity_columns_have_their_base_type() {
        assert_eq!(sql_type("int identity", 4, 10, 0), "int");
        assert_eq!(sql_type("bigint IDENTITY", 8, 19, 0), "bigint");
        assert_eq!(sql_type("decimal() identity", 14, 12, 0), "decimal(12,0)");
        assert_eq!(sql_type("numeric() identity", 11, 9, 0), "numeric(9,0)");
        assert_eq!(
            sql_type("identity", 0, 0, 0),
            "identity",
            "not a suffix on its own"
        );
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
    fn the_json_checker_accepts_json_and_rejects_the_rest() {
        for valid in [
            "[]",
            "{}",
            " [1, -0.5, 2e10, 3E-2, true, false, null] ",
            "{\"a\":{\"b\":[\"\\u00e9\\n\",{}]}}",
            "\"x\"",
            "0",
        ] {
            assert!(is_valid_json(valid), "{valid}");
        }
        for invalid in [
            "",
            "{\"a\":1},{\"a\":2}",
            "[1,]",
            "{\"a\" 1}",
            "[01]",
            "[1.]",
            "\"a\nb\"",
            "\"\\x\"",
            "[tru]",
            "{\"a\":1",
            "nul",
        ] {
            assert!(!is_valid_json(invalid), "{invalid}");
        }
        let deep = format!(
            "{}{}",
            "[".repeat(MAX_JSON_DEPTH + 2),
            "]".repeat(MAX_JSON_DEPTH + 2)
        );
        assert!(!is_valid_json(&deep));
    }

    #[test]
    fn the_platform_is_a_runtime_identifier() {
        assert!(
            [
                "win-x64",
                "win-x86",
                "win-arm64",
                "linux-x64",
                "linux-arm64",
                "linux-musl-x64",
                "linux-musl-arm64",
                "osx-x64",
                "osx-arm64",
            ]
            .contains(&PLATFORM),
            "{PLATFORM}"
        );
    }
}

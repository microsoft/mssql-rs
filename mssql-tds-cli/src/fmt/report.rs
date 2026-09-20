// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Server messages and row counts, in the reference's wording.

use mssql_tds::error::{SqlErrorInfo, SqlInfoMessage};

use crate::messages::EOL;

/// What `-j` leaves in front of a server message. The reference names the ODBC
/// driver here; we name ourselves, since that is what actually produced it.
const DRIVER_PREFIX: &str = "[Microsoft][Rust Driver for SQL Server][SQL Server]";

/// A server message, whatever token carried it.
#[derive(Debug, Clone)]
pub struct Message {
    pub number: u32,
    pub state: u8,
    pub severity: i32,
    pub server: Option<String>,
    pub procedure: Option<String>,
    pub line: Option<i32>,
    pub text: String,
}

impl From<&SqlErrorInfo> for Message {
    fn from(e: &SqlErrorInfo) -> Self {
        Self {
            number: e.number,
            state: e.state,
            severity: e.class,
            server: e.server_name.clone(),
            procedure: e.proc_name.clone(),
            line: e.line_number,
            text: e.message.clone(),
        }
    }
}

impl From<&SqlInfoMessage> for Message {
    fn from(m: &SqlInfoMessage) -> Self {
        Self {
            number: m.number,
            state: m.state,
            severity: m.class,
            server: m.server_name.clone(),
            procedure: m.proc_name.clone(),
            line: m.line_number,
            text: m.message.clone(),
        }
    }
}

impl Message {
    /// `PRINT` output and other severity-10 chatter is printed bare; anything
    /// that counts as an error gets the `Msg ...` header.
    pub fn is_error(&self) -> bool {
        self.severity > 10
    }

    /// `PRINT` arrives as an info token numbered zero, where `RAISERROR` always
    /// carries a number even at severity zero. The two are otherwise identical
    /// on the wire, and every `-m` rule below turns on telling them apart:
    /// `PRINT` is program output rather than a message, so no threshold hides
    /// it and no `-m -1` gives it a header.
    pub fn is_print(&self) -> bool {
        self.number == 0
    }

    /// The rendered form, already newline-terminated.
    ///
    /// `raw` is `-j`: the reference normally strips the driver's own prefix
    /// from the message text and `-j` leaves it on.
    ///
    /// `force_header` is ODBC's `-m -1`, which puts the `Msg ...` header on
    /// messages that would otherwise print bare.
    pub fn render(&self, raw: bool, force_header: bool) -> String {
        let text = if raw {
            format!("{DRIVER_PREFIX}{}", self.text)
        } else {
            self.text.clone()
        };

        // An error always gets the header. `-m -1` extends it to the numbered
        // messages that would otherwise print bare, but never to `PRINT`.
        let wants_header = self.is_error() || (force_header && !self.is_print());
        if !wants_header {
            return format!("{text}{EOL}");
        }

        let server = self.server.as_deref().unwrap_or("");
        let line = self.line.unwrap_or(0);
        let header = match self.procedure.as_deref() {
            Some(proc_name) if !proc_name.is_empty() => format!(
                "Msg {}, Level {}, State {}, Server {}, Procedure {}, Line {}",
                self.number, self.severity, self.state, server, proc_name, line
            ),
            _ => format!(
                "Msg {}, Level {}, State {}, Server {}, Line {}",
                self.number, self.severity, self.state, server, line
            ),
        };
        format!("{header}{EOL}{text}{EOL}")
    }
}

/// `MSG_ROWS_AFFECTED`.
///
/// ODBC never singularises the count; go-sqlcmd writes "1 row affected".
pub fn rows_affected(count: u64, compat: crate::compat::Compat) -> String {
    let noun = if compat.is_go() && count == 1 {
        "row"
    } else {
        "rows"
    };
    format!("{EOL}({count} {noun} affected){EOL}")
}

/// `MSG_PERF_STATS` — the `-p` block, printed after each batch.
pub fn perf_stats(packet_size: u32, transactions: u64, elapsed_ms: u64) -> String {
    let (avg, per_second) = rates(transactions, elapsed_ms);
    format!(
        "{EOL}Network packet size (bytes): {packet_size}{EOL}\
         {transactions} xact[s]:{EOL}\
         Clock Time (ms.): total   {elapsed_ms:>7}  avg   {avg} ({per_second} xacts per sec.){EOL}"
    )
}

/// `MSG_PERF_STATS_COLON` — the `-p1` machine-readable form, which the
/// reference terminates with a trailing space before the newline.
pub fn perf_stats_colon(packet_size: u32, transactions: u64, elapsed_ms: u64) -> String {
    let (avg, per_second) = rates(transactions, elapsed_ms);
    format!("{EOL}{packet_size}:{transactions}:{elapsed_ms}:{avg}:{per_second} {EOL}")
}

/// Mean milliseconds per transaction, and transactions per second. A batch that
/// takes no measurable time would divide by zero, so it counts as one
/// millisecond — which is what the reference reports for a trivial query.
fn rates(transactions: u64, elapsed_ms: u64) -> (String, String) {
    let elapsed = elapsed_ms.max(1) as f64;
    let count = transactions.max(1) as f64;
    (
        format!("{:.2}", elapsed / count),
        format!("{:.2}", count * 1000.0 / elapsed),
    )
}

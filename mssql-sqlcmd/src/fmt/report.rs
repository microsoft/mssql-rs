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

    /// The rendered form, already newline-terminated.
    ///
    /// `raw` is `-j`: the reference normally strips the driver's own prefix
    /// from the message text and `-j` leaves it on.
    pub fn render(&self, raw: bool) -> String {
        let text = if raw {
            format!("{DRIVER_PREFIX}{}", self.text)
        } else {
            self.text.clone()
        };

        if !self.is_error() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn message(severity: i32, procedure: Option<&str>) -> Message {
        Message {
            number: 50000,
            state: 1,
            severity,
            server: Some("SRV".into()),
            procedure: procedure.map(str::to_string),
            line: Some(1),
            text: "boom".into(),
        }
    }

    #[test]
    fn errors_carry_the_msg_header() {
        assert_eq!(
            message(16, None).render(false),
            format!("Msg 50000, Level 16, State 1, Server SRV, Line 1{EOL}boom{EOL}")
        );
    }

    #[test]
    fn a_procedure_name_adds_a_field() {
        assert_eq!(
            message(16, Some("dbo.p")).render(false),
            format!(
                "Msg 50000, Level 16, State 1, Server SRV, Procedure dbo.p, Line 1{EOL}boom{EOL}"
            )
        );
    }

    #[test]
    fn an_empty_procedure_name_is_omitted_rather_than_printed_blank() {
        assert_eq!(
            message(16, Some("")).render(false),
            format!("Msg 50000, Level 16, State 1, Server SRV, Line 1{EOL}boom{EOL}")
        );
    }

    #[test]
    fn print_output_is_printed_bare() {
        assert_eq!(message(10, None).render(false), format!("boom{EOL}"));
        assert_eq!(message(0, None).render(false), format!("boom{EOL}"));
    }

    #[test]
    fn raw_mode_keeps_the_driver_prefix() {
        assert!(message(16, None).render(true).contains("[SQL Server]boom"));
        assert!(message(10, None).render(true).contains("[SQL Server]boom"));
    }

    #[test]
    fn the_statistics_block_pads_the_total_to_seven_columns() {
        assert_eq!(
            perf_stats(4096, 1, 1),
            format!(
                "{EOL}Network packet size (bytes): 4096{EOL}1 xact[s]:{EOL}\
                 Clock Time (ms.): total         1  avg   1.00 (1000.00 xacts per sec.){EOL}"
            )
        );
    }

    #[test]
    fn the_colon_form_ends_with_a_trailing_space() {
        assert_eq!(
            perf_stats_colon(4096, 1, 1),
            format!("{EOL}4096:1:1:1.00:1000.00 {EOL}")
        );
    }

    #[test]
    fn an_immeasurably_fast_batch_does_not_divide_by_zero() {
        assert_eq!(rates(0, 0), ("1.00".to_string(), "1000.00".to_string()));
    }

    #[test]
    fn odbc_never_singularises_the_row_count() {
        use crate::compat::Compat;
        assert_eq!(
            rows_affected(1, Compat::Odbc),
            format!("{EOL}(1 rows affected){EOL}")
        );
        assert_eq!(
            rows_affected(0, Compat::Odbc),
            format!("{EOL}(0 rows affected){EOL}")
        );
    }

    #[test]
    fn go_singularises_exactly_one_row() {
        use crate::compat::Compat;
        assert_eq!(
            rows_affected(1, Compat::Go),
            format!("{EOL}(1 row affected){EOL}")
        );
        assert_eq!(
            rows_affected(0, Compat::Go),
            format!("{EOL}(0 rows affected){EOL}")
        );
        assert_eq!(
            rows_affected(2, Compat::Go),
            format!("{EOL}(2 rows affected){EOL}")
        );
    }
}

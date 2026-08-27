// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running a batch and rendering whatever comes back.

use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
use mssql_tds::core::CancelHandle;
use mssql_tds::error::Error as TdsError;

use crate::compat::Compat;
use crate::fmt::color;
use crate::fmt::layout::{self, Format};
use crate::fmt::report::{self, Message};
use crate::fmt::table::{Table, TableStyle};
use crate::fmt::value;
use crate::fmt::widths;
use crate::messages::EOL;

/// Where each piece of output belongs. The caller owns the sinks, so the runner
/// hands back a classified stream rather than writing anything itself.
#[derive(Debug)]
pub enum Output {
    /// Rows, headings and row counts.
    Result(String),
    /// Server messages, which `-r` may route elsewhere.
    Message(Message),
}

#[derive(Debug, Clone)]
pub struct RunStyle {
    pub table: TableStyle,
    pub max_var_width: usize,
    pub max_fixed_width: usize,
    /// `:xml on` — emit each cell's text with no heading, padding or row count,
    /// so an XML result set comes back as the document the server sent.
    pub xml: bool,
    /// `--vertical` / `--ascii` / `SQLCMDFORMAT`.
    pub format: Format,
    /// Whose behaviour to follow where the two tools disagree.
    pub compat: Compat,
    /// `-R` — render money, `decimal`/`numeric` and timestamps with the
    /// client's regional settings.
    pub regional: bool,
    /// `SQLCMDCOLORSCHEME`. Inactive unless a scheme is named and the results
    /// are going to a terminal.
    pub colors: crate::fmt::color::Colorizer,
}

impl RunStyle {
    fn values(&self) -> crate::fmt::value::ValueStyle {
        crate::fmt::value::ValueStyle {
            compat: self.compat,
            regional: self.regional,
        }
    }
}

/// Outcome of one batch, used for `-b`, `-V` and the final exit code.
#[derive(Debug, Default)]
pub struct Outcome {
    pub output: Vec<Output>,
    pub highest_severity: i32,
    pub error_number: u32,
    pub failed: bool,
    /// First cell of the first row of the first result set, which is what
    /// `:exit(query)` turns into an exit code.
    pub first_cell: Option<mssql_tds::datatypes::column_values::ColumnValues>,
    /// Wall-clock time the batch took, for `-p`.
    pub elapsed_ms: u64,
    /// Packet size the connection negotiated, also for `-p`.
    pub packet_size: u32,
    /// Set when the layout already ends in a rule, so the row count that
    /// follows should not be preceded by a blank line.
    pub suppress_count_gap: bool,
    /// Whether a row count has been emitted for the current result set.
    counted: bool,
    /// Whether the current result set drew any rows.
    drew_rows: bool,
}

impl Outcome {
    fn note(&mut self, message: Message) {
        if message.is_error() && message.severity > self.highest_severity {
            self.highest_severity = message.severity;
            self.error_number = message.number;
        }
        self.output.push(Output::Message(message));
    }
}

/// Executes `sql` and drains every result set it produces.
pub async fn run(
    client: &mut TdsClient,
    sql: &str,
    timeout: Option<u32>,
    cancel: Option<&CancelHandle>,
    style: &RunStyle,
) -> Outcome {
    let mut outcome = Outcome::default();
    let started = std::time::Instant::now();
    outcome.packet_size = client.packet_size();

    if let Err(error) = client.execute(sql.to_string(), timeout, cancel).await {
        drain_messages(client, &mut outcome);
        record_error(&error, &mut outcome);
        outcome.elapsed_ms = started.elapsed().as_millis() as u64;
        return outcome;
    }

    loop {
        drain_messages(client, &mut outcome);

        let has_rows = !client.get_metadata().is_empty();
        if has_rows && let Err(error) = render_result_set(client, style, &mut outcome).await {
            drain_messages(client, &mut outcome);
            record_error(&error, &mut outcome);
            break;
        }
        report_counts(client, &mut outcome, style);

        match client.move_to_next().await {
            Ok(true) => continue,
            Ok(false) => break,
            Err(error) => {
                drain_messages(client, &mut outcome);
                record_error(&error, &mut outcome);
                break;
            }
        }
    }

    drain_messages(client, &mut outcome);
    let _ = client.close_query().await;
    drain_messages(client, &mut outcome);
    report_counts(client, &mut outcome, style);
    outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    outcome
}

/// Emits `(N rows affected)` for each statement the server reported a count
/// for. A statement under `SET NOCOUNT ON` reports none and prints nothing,
/// and `:xml on` suppresses the line entirely so the document stands alone.
fn report_counts(client: &mut TdsClient, outcome: &mut Outcome, style: &RunStyle) {
    let counts = client.take_done_row_counts();
    if style.xml {
        return;
    }
    for count in counts.into_iter().flatten() {
        let text = report::rows_affected(count, style.compat);
        let text = if outcome.suppress_count_gap {
            text.trim_start_matches(EOL).to_string()
        } else {
            text
        };
        // The sentence is coloured; the blank line before it and the
        // terminator after it are not, which is where go-sqlcmd draws the line.
        let lead = text.len() - text.trim_start_matches(EOL).len();
        let (lead, rest) = text.split_at(lead);
        let (sentence, tail) = match rest.strip_suffix(EOL) {
            Some(sentence) => (sentence, EOL),
            None => (rest, ""),
        };
        let painted = style.colors.paint(sentence, color::TextType::Warning);
        outcome
            .output
            .push(Output::Result(format!("{lead}{painted}{tail}")));
        outcome.counted = true;
    }

    // go-sqlcmd ends a result set with a blank line whether or not a count
    // followed it, so a statement under `SET NOCOUNT ON` still gets one.
    if style.compat.is_go() && !outcome.counted && outcome.drew_rows {
        outcome.output.push(Output::Result(EOL.to_string()));
        outcome.drew_rows = false;
    }
}

async fn render_result_set(
    client: &mut TdsClient,
    style: &RunStyle,
    outcome: &mut Outcome,
) -> Result<(), TdsError> {
    let columns = client.get_metadata().clone();

    if style.xml {
        let mut buffer = String::new();
        while let Some(row) = client.next_row().await? {
            if outcome.first_cell.is_none() {
                outcome.first_cell = row.first().cloned();
            }
            for (cell, column) in row.iter().zip(&columns) {
                buffer.push_str(&value::render(cell, column, style.values()));
            }
        }
        if !buffer.is_empty() {
            buffer.push_str(EOL);
        }
        outcome.output.push(Output::Result(buffer));
        return Ok(());
    }

    let layouts = columns
        .iter()
        .map(|column| {
            widths::layout(
                column,
                style.max_var_width,
                style.max_fixed_width,
                style.compat,
            )
        })
        .collect();
    let mut table = Table::new(&columns, layouts, style.table.clone());

    let mut rows = 0u64;
    let mut buffer = String::new();
    // The alternative layouts need every row measured before any is drawn.
    let mut collected: Vec<Vec<String>> = Vec::new();
    let tabular = style.format == Format::Horizontal;

    while let Some(row) = client.next_row().await? {
        // The very first cell of the batch is what `:exit(query)` reports.
        if outcome.first_cell.is_none() {
            outcome.first_cell = row.first().cloned();
        }
        let cells: Vec<String> = row
            .iter()
            .zip(&columns)
            .map(|(cell, column)| value::render(cell, column, style.values()))
            .collect();
        if tabular {
            for line in table.row(&cells) {
                buffer.push_str(&line);
                buffer.push_str(EOL);
            }
        } else {
            collected.push(cells);
        }
        rows += 1;
    }

    match style.format {
        Format::Horizontal => {
            if rows == 0 {
                for line in table.header_only() {
                    buffer.push_str(&line);
                    buffer.push_str(EOL);
                }
            }
        }
        Format::Vertical => {
            buffer.push_str(&layout::vertical(
                &columns,
                &collected,
                style.table.headers,
                &style.colors,
            ));
        }
        Format::Ascii => {
            let numeric: Vec<bool> = columns.iter().map(widths::is_right_justified).collect();
            buffer.push_str(&layout::ascii(
                &columns,
                &collected,
                &style.table.separator,
                &numeric,
                &style.colors,
            ));
            // The bordered table already closes with a rule, so go-sqlcmd runs
            // the row count straight on without the usual blank line. Trimming
            // the leading newline the count carries reproduces that.
            outcome.output.push(Output::Result(buffer));
            outcome.suppress_count_gap = true;
            return Ok(());
        }
    }

    outcome.output.push(Output::Result(buffer));
    outcome.drew_rows = rows > 0;
    outcome.counted = false;
    Ok(())
}

/// Info tokens and deferred errors accumulate on the client; move them across
/// as they appear so `PRINT` output and `Msg` lines land near the statement
/// that produced them.
fn drain_messages(client: &mut TdsClient, outcome: &mut Outcome) {
    for message in client.take_info_messages() {
        outcome.note(Message::from(&message));
    }
    // A statement that failed part-way through a batch does not end it: the
    // driver collects the error and carries on, so it arrives here rather than
    // as an `Err` from the call that was in flight.
    for error in client.take_pending_errors() {
        outcome.failed = true;
        outcome.note(Message::from(&error));
    }
}

/// A server error arrives as a driver error carrying the original diagnostics;
/// anything else is a client-side failure and is reported as such.
fn record_error(error: &TdsError, outcome: &mut Outcome) {
    outcome.failed = true;
    match error {
        TdsError::SqlServerError { diagnostics } => {
            for info in &diagnostics.info_messages {
                outcome.note(Message::from(info));
            }
            for server_error in &diagnostics.errors {
                outcome.note(Message::from(server_error));
            }
        }
        other => outcome.note(Message {
            number: 0,
            state: 0,
            severity: 16,
            server: None,
            procedure: None,
            line: None,
            text: other.to_string(),
        }),
    }
}

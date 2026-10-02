// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! JSON output for `--format json`.
//!
//! Native sqlcmd runs the batches as usual and, instead of printing text, hands
//! each piece of output to a [`JsonDocument`]: result sets and their rows, row
//! counts, and server messages. When sqlcmd exits it calls
//! [`JsonDocument::render`] with the connection details and the exit code, and
//! prints the one JSON document that returns:
//!
//! ```json
//! {
//!   "sqlcmd": {
//!     "version": "18.5.1.1"
//!   },
//!   "connection": {
//!     "server": "localhost",
//!     "database": "master",
//!     "authentication": "SqlPassword",
//!     "encrypt": true
//!   },
//!   "exitCode": 0,
//!   "output": [
//!     {
//!       "type": "resultSet",
//!       "columns": ["id", "name"],
//!       "rows": [
//!         ["1", "a"],
//!         ["2", null]
//!       ]
//!     },
//!     {
//!       "type": "rowsAffected",
//!       "count": 2
//!     }
//!   ]
//! }
//! ```
//!
//! Values are strings, exactly as sqlcmd would print them, so no precision is
//! lost and every SQL type has a representation. SQL `NULL` is JSON `null`,
//! which keeps it distinct from the string `"NULL"`. Output keeps the order in
//! which the server produced it.

use std::fmt::Write;

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

/// A server message or error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// `true` for an error, `false` for an informational message such as
    /// `PRINT`.
    pub is_error: bool,
    pub number: i32,
    pub state: i32,
    pub severity: i32,
    pub text: String,
}

/// Why a call could not be applied to the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// A row arrived before any result set was started.
    NoResultSet,
    /// A row's value count differs from the result set's column count.
    ColumnCountMismatch { expected: usize, actual: usize },
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::NoResultSet => write!(f, "a row was added before any result set"),
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
    ResultSet {
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    },
    RowsAffected(i64),
    Message(Message),
}

/// Collects one invocation's output, in order, and renders it as JSON.
#[derive(Debug, Default)]
pub struct JsonDocument {
    output: Vec<Output>,
    /// Index in `output` of the result set rows are added to. A message can
    /// arrive between two rows of the same result set, so this is not always
    /// the last entry.
    current_result_set: Option<usize>,
}

impl JsonDocument {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a result set; the rows that follow belong to it.
    pub fn begin_result_set(&mut self, columns: Vec<String>) {
        self.current_result_set = Some(self.output.len());
        self.output.push(Output::ResultSet {
            columns,
            rows: Vec::new(),
        });
    }

    /// Adds a row to the current result set. `None` is SQL `NULL`.
    pub fn add_row(&mut self, values: Vec<Option<String>>) -> Result<(), FormatError> {
        let Some(Output::ResultSet { columns, rows }) = self
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

    /// Records a statement's "(n rows affected)".
    pub fn add_rows_affected(&mut self, count: i64) {
        self.output.push(Output::RowsAffected(count));
    }

    /// Records a server message or error.
    pub fn add_message(&mut self, message: Message) {
        self.output.push(Output::Message(message));
    }

    /// Renders the whole document, ending with a newline.
    pub fn render(&self, version: &str, connection: &Connection, exit_code: i32) -> String {
        let mut out = String::new();
        out.push_str("{\n  \"sqlcmd\": {\n    \"version\": ");
        push_string(&mut out, version);
        out.push_str("\n  },\n  \"connection\": {\n    \"server\": ");
        push_optional_string(&mut out, connection.server.as_deref());
        out.push_str(",\n    \"database\": ");
        push_optional_string(&mut out, connection.database.as_deref());
        out.push_str(",\n    \"authentication\": ");
        push_optional_string(&mut out, connection.authentication.as_deref());
        out.push_str(",\n    \"encrypt\": ");
        out.push_str(if connection.encrypt { "true" } else { "false" });
        out.push_str("\n  },\n  \"exitCode\": ");
        let _ = write!(out, "{exit_code}");
        out.push_str(",\n  \"output\": [");
        for (index, item) in self.output.iter().enumerate() {
            out.push_str(if index == 0 { "\n" } else { ",\n" });
            push_output(&mut out, item);
        }
        out.push_str(if self.output.is_empty() {
            "]\n}\n"
        } else {
            "\n  ]\n}\n"
        });
        out
    }
}

fn push_output(out: &mut String, item: &Output) {
    match item {
        Output::ResultSet { columns, rows } => {
            out.push_str("    {\n      \"type\": \"resultSet\",\n      \"columns\": ");
            push_array(out, columns.iter().map(|column| Some(column.as_str())));
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
                ",\n      \"number\": {},\n      \"state\": {},\n      \"severity\": {},\n      \"message\": ",
                message.number, message.state, message.severity
            );
            push_string(out, &message.text);
            out.push_str("\n    }");
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

    fn connection() -> Connection {
        Connection {
            server: Some("localhost".to_string()),
            database: Some("master".to_string()),
            authentication: Some("SqlPassword".to_string()),
            encrypt: true,
        }
    }

    #[test]
    fn renders_the_header_and_an_empty_output() {
        let rendered = JsonDocument::new().render("18.5.1.1", &connection(), 1);
        assert_eq!(
            rendered,
            "{\n  \"sqlcmd\": {\n    \"version\": \"18.5.1.1\"\n  },\n  \"connection\": {\n    \
             \"server\": \"localhost\",\n    \"database\": \"master\",\n    \
             \"authentication\": \"SqlPassword\",\n    \"encrypt\": true\n  },\n  \
             \"exitCode\": 1,\n  \"output\": []\n}\n"
        );
    }

    #[test]
    fn renders_result_sets_counts_and_messages_in_order() {
        let mut document = JsonDocument::new();
        document.begin_result_set(vec!["id".to_string(), "name".to_string()]);
        document
            .add_row(vec![Some("1".to_string()), Some("a".to_string())])
            .unwrap();
        document.add_row(vec![Some("2".to_string()), None]).unwrap();
        document.add_rows_affected(2);
        document.add_message(Message {
            is_error: true,
            number: 50000,
            state: 1,
            severity: 16,
            text: "boom".to_string(),
        });

        let rendered = document.render("18.5.1.1", &connection(), 1);
        let output = rendered.split("\"output\": ").nth(1).unwrap();
        assert_eq!(
            output,
            "[\n    {\n      \"type\": \"resultSet\",\n      \"columns\": [\"id\", \"name\"],\n      \
             \"rows\": [\n        [\"1\", \"a\"],\n        [\"2\", null]\n      ]\n    },\n    \
             {\n      \"type\": \"rowsAffected\",\n      \"count\": 2\n    },\n    \
             {\n      \"type\": \"error\",\n      \"number\": 50000,\n      \"state\": 1,\n      \
             \"severity\": 16,\n      \"message\": \"boom\"\n    }\n  ]\n}\n"
        );
    }

    /// SQL `NULL` and the string "NULL" must stay distinguishable.
    #[test]
    fn null_is_json_null_and_the_string_null_is_a_string() {
        let mut document = JsonDocument::new();
        document.begin_result_set(vec!["v".to_string()]);
        document.add_row(vec![None]).unwrap();
        document.add_row(vec![Some("NULL".to_string())]).unwrap();

        let rendered = document.render("v", &Connection::default(), 0);
        assert!(
            rendered.contains("[null],\n        [\"NULL\"]"),
            "{rendered}"
        );
    }

    #[test]
    fn unknown_connection_fields_are_null() {
        let rendered = JsonDocument::new().render("v", &Connection::default(), 0);
        assert!(rendered.contains("\"server\": null"));
        assert!(rendered.contains("\"database\": null"));
        assert!(rendered.contains("\"authentication\": null"));
        assert!(rendered.contains("\"encrypt\": false"));
    }

    #[test]
    fn an_empty_result_set_has_empty_rows() {
        let mut document = JsonDocument::new();
        document.begin_result_set(vec!["v".to_string()]);
        let rendered = document.render("v", &Connection::default(), 0);
        assert!(rendered.contains("\"columns\": [\"v\"],\n      \"rows\": []\n"));
    }

    #[test]
    fn strings_are_escaped() {
        let mut out = String::new();
        push_string(&mut out, "q\"b\\n\nr\rt\tb\u{08}f\u{0C}c\u{01}é\u{1F600}");
        assert_eq!(out, "\"q\\\"b\\\\n\\nr\\rt\\tb\\bf\\fc\\u0001é\u{1F600}\"");
    }

    #[test]
    fn a_row_before_any_result_set_is_rejected() {
        let mut document = JsonDocument::new();
        assert_eq!(
            document.add_row(vec![Some("1".to_string())]),
            Err(FormatError::NoResultSet)
        );
    }

    #[test]
    fn a_row_with_the_wrong_value_count_is_rejected() {
        let mut document = JsonDocument::new();
        document.begin_result_set(vec!["a".to_string(), "b".to_string()]);
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
        let mut document = JsonDocument::new();
        document.begin_result_set(vec!["v".to_string()]);
        document.add_row(vec![Some("1".to_string())]).unwrap();
        document.add_message(Message {
            is_error: false,
            number: 0,
            state: 1,
            severity: 0,
            text: "warning".to_string(),
        });
        document.add_row(vec![Some("2".to_string())]).unwrap();

        let rendered = document.render("v", &Connection::default(), 0);
        assert!(rendered.contains("[\"1\"],\n        [\"2\"]"), "{rendered}");
    }
}

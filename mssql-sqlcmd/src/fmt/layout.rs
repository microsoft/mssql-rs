// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Output layouts beyond the ODBC tabular one.
//!
//! go-sqlcmd adds a vertical layout, which prints one field per line, and an
//! ASCII-art table. Both are selected with their own flag or through
//! `SQLCMDFORMAT`.

use crate::messages::EOL;

use super::color::{Colorizer, TextType};
use mssql_tds::query::metadata::ColumnMetadata;

/// Which layout the results are drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// The ODBC layout: a heading, a dashed rule, then padded rows.
    #[default]
    Horizontal,
    /// One `name value` line per field, with a blank line between rows.
    Vertical,
    /// A `+---+` bordered table.
    Ascii,
}

impl Format {
    /// Parses a `SQLCMDFORMAT` value. Anything unrecognised means horizontal,
    /// which is what go-sqlcmd does rather than reporting an error.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "vert" | "vertical" => Format::Vertical,
            "ascii" => Format::Ascii,
            _ => Format::Horizontal,
        }
    }
}

/// Renders one result set vertically.
///
/// Field names are padded to the longest name in the set so the values line up,
/// and a blank line separates one row from the next.
pub fn vertical(
    columns: &[ColumnMetadata],
    rows: &[Vec<String>],
    headers: i64,
    colors: &Colorizer,
) -> String {
    let width = columns
        .iter()
        .map(|c| c.column_name.chars().count())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for row in rows {
        for (cell, column) in row.iter().zip(columns) {
            // `-h -1` drops the names and leaves the bare values.
            if headers > -1 {
                let name = &column.column_name;
                let pad = " ".repeat(width - name.chars().count() + 1);
                out.push_str(&colors.paint(name, TextType::Header));
                out.push_str(&pad);
            }
            out.push_str(&colors.paint(cell, TextType::Cell));
            out.push_str(EOL);
        }
        out.push_str(EOL);
    }
    out
}

/// Renders one result set as a bordered ASCII table.
///
/// `separator` is `-s`; a blank or plain-space separator falls back to `|` so
/// the borders stay visible.
pub fn ascii(
    columns: &[ColumnMetadata],
    rows: &[Vec<String>],
    separator: &str,
    numeric: &[bool],
    colors: &Colorizer,
) -> String {
    let sep = match separator {
        "" | " " => "|",
        other => other,
    };

    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let widest_cell = rows
                .iter()
                .filter_map(|row| row.get(i))
                .map(|cell| cell.trim_end().chars().count())
                .max()
                .unwrap_or(0);
            widest_cell.max(column.column_name.chars().count())
        })
        .collect();

    let divider = {
        let mut line = String::from("+");
        for width in &widths {
            line.push_str(&"-".repeat(width + 2));
            line.push('+');
        }
        colors.paint(&line, TextType::Separator)
    };
    let sep = colors.paint(sep, TextType::Separator);
    let sep = sep.as_str();

    let mut out = String::new();
    out.push_str(&divider);
    out.push_str(EOL);

    out.push_str(sep);
    for (column, width) in columns.iter().zip(&widths) {
        out.push(' ');
        out.push_str(&colors.paint(&pad_right(&column.column_name, *width), TextType::Header));
        out.push(' ');
        out.push_str(sep);
    }
    out.push_str(EOL);
    out.push_str(&divider);
    out.push_str(EOL);

    for row in rows {
        out.push_str(sep);
        for (i, width) in widths.iter().enumerate() {
            let cell = row.get(i).map(|c| c.trim_end()).unwrap_or("");
            out.push(' ');
            // Numbers line up on the right, as they do in the ODBC layout.
            let padded = if numeric.get(i).copied().unwrap_or(false) {
                pad_left(cell, *width)
            } else {
                pad_right(cell, *width)
            };
            out.push_str(&colors.paint(&padded, TextType::Cell));
            out.push(' ');
            out.push_str(sep);
        }
        out.push_str(EOL);
    }

    out.push_str(&divider);
    out.push_str(EOL);
    out
}

fn pad_right(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.chars().take(width).collect();
    }
    format!("{text}{}", " ".repeat(width - len))
}

fn pad_left(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.chars().take(width).collect();
    }
    format!("{}{text}", " ".repeat(width - len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_names_are_recognised_in_both_spellings() {
        assert_eq!(Format::parse("vert"), Format::Vertical);
        assert_eq!(Format::parse("vertical"), Format::Vertical);
        assert_eq!(Format::parse("ASCII"), Format::Ascii);
        assert_eq!(Format::parse("horiz"), Format::Horizontal);
        assert_eq!(Format::parse("nonsense"), Format::Horizontal);
    }

    #[test]
    fn padding_truncates_rather_than_overflowing() {
        assert_eq!(pad_right("abcdef", 3), "abc");
        assert_eq!(pad_left("abcdef", 3), "abc");
        assert_eq!(pad_right("ab", 4), "ab  ");
        assert_eq!(pad_left("ab", 4), "  ab");
    }
}

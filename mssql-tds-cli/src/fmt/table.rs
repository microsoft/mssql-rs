// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Laying rows out as the reference does: a heading, a dashed rule, then rows,
//! each field padded to the column width and joined by the column separator.

use mssql_tds::query::metadata::ColumnMetadata;

use super::color::{Colorizer, TextType};
use super::widths::{self, Align, ColumnLayout};
use crate::cli::validate::ControlChars;

/// Everything the layout depends on, gathered so the renderer stays a pure
/// function of options plus data.
#[derive(Debug, Clone)]
pub struct TableStyle {
    pub separator: String,
    /// `-h`: 0 prints the heading once, `n > 0` every `n` rows, -1 never.
    pub headers: i64,
    /// `-w`: wrap the rendered line at this many columns.
    pub screen_width: usize,
    /// `-W`: emit fields at their natural length instead of padding them.
    pub trim: bool,
    pub control_chars: Option<ControlChars>,
    /// Whether a repeated heading is preceded by a blank line, as go-sqlcmd
    /// does and ODBC does not.
    pub gap_before_repeat: bool,
    /// `SQLCMDCOLORSCHEME`. Inactive unless a scheme is named and the results
    /// are going to a terminal.
    pub colors: Colorizer,
}

pub struct Table {
    layouts: Vec<ColumnLayout>,
    names: Vec<String>,
    style: TableStyle,
    rows_since_header: i64,
    header_written: bool,
}

impl Table {
    pub fn new(columns: &[ColumnMetadata], layouts: Vec<ColumnLayout>, style: TableStyle) -> Self {
        Self {
            layouts,
            names: columns.iter().map(|c| c.column_name.clone()).collect(),
            style,
            rows_since_header: 0,
            header_written: false,
        }
    }

    /// Heading plus rule, or nothing when `-h -1` suppressed them.
    fn header_lines(&self) -> Vec<String> {
        // Headings are left-justified even above a right-justified column.
        let heading = self.join(self.names.iter().zip(&self.layouts).map(|(name, layout)| {
            pad(
                name,
                &ColumnLayout {
                    width: layout.width,
                    align: Align::Left,
                },
                self.style.trim,
            )
        }));
        let rule = self.join(self.names.iter().zip(&self.layouts).map(|(name, layout)| {
            // Under `-W` the rule matches the heading text, not the column width.
            let width = if self.style.trim {
                name.chars().count()
            } else {
                layout.width
            };
            "-".repeat(width)
        }));
        vec![
            self.style.colors.paint(&heading, TextType::Header),
            self.style.colors.paint(&rule, TextType::Separator),
        ]
    }

    fn join(&self, fields: impl Iterator<Item = String>) -> String {
        fields.collect::<Vec<_>>().join(&self.style.separator)
    }

    /// Joins fields for a data row, colouring each field and each separator
    /// separately.
    ///
    /// The heading and the rule are each drawn as a single coloured run, but a
    /// data row is not: the reference wraps every cell and every separator in
    /// its own escape sequence. Measured through a PTY.
    fn join_row(&self, fields: impl Iterator<Item = String>) -> String {
        if !self.style.colors.is_active() {
            return self.join(fields);
        }
        let separator = self
            .style
            .colors
            .paint(&self.style.separator, TextType::Separator);
        fields
            .map(|field| self.style.colors.paint(&field, TextType::Cell))
            .collect::<Vec<_>>()
            .join(&separator)
    }

    /// Renders one row, prefixed by a heading when one is due.
    pub fn row(&mut self, cells: &[String]) -> Vec<String> {
        let mut out = Vec::new();

        let due = match self.style.headers {
            n if n < 0 => false,
            0 => !self.header_written,
            n => self.rows_since_header % n == 0,
        };
        if due {
            let first = !self.header_written;
            out.extend(self.header_lines().into_iter().flat_map(|l| self.wrap(l)));
            // go-sqlcmd follows a *repeated* heading with a blank line; the
            // first heading of a result set is not set off that way.
            if !first && self.style.gap_before_repeat {
                out.push(String::new());
            }
            self.header_written = true;
            self.rows_since_header = 0;
        }
        self.rows_since_header += 1;

        let line = self.join_row(cells.iter().zip(&self.layouts).map(|(cell, layout)| {
            pad(
                &clean(cell, self.style.control_chars),
                layout,
                self.style.trim,
            )
        }));
        out.extend(self.wrap(line));
        out
    }

    /// Emits the heading for a result set that returned no rows at all.
    pub fn header_only(&mut self) -> Vec<String> {
        if self.style.headers < 0 {
            return Vec::new();
        }
        self.header_written = true;
        self.header_lines()
            .into_iter()
            .flat_map(|line| self.wrap(line))
            .collect()
    }

    /// `-w` wraps rather than truncates: the line is cut into screen-width chunks.
    fn wrap(&self, line: String) -> Vec<String> {
        let width = self.style.screen_width;
        if width == 0 || line.chars().count() <= width {
            return vec![line];
        }
        let chars: Vec<char> = line.chars().collect();
        chars
            .chunks(width)
            .map(|chunk| chunk.iter().collect())
            .collect()
    }
}

/// Pads a field to its column width, or leaves it alone under `-W` or when the
/// column has no width limit.
fn pad(text: &str, layout: &ColumnLayout, trim: bool) -> String {
    if trim || layout.width == widths::NATURAL_WIDTH {
        return text.to_string();
    }
    let width = layout.width;
    let len = text.chars().count();
    if len >= width {
        return text.chars().take(width).collect();
    }
    let fill = " ".repeat(width - len);
    match layout.align {
        Align::Left => format!("{text}{fill}"),
        Align::Right => format!("{fill}{text}"),
    }
}

/// `-k` family: strip control characters, or replace them with spaces.
fn clean(text: &str, mode: Option<ControlChars>) -> String {
    let Some(mode) = mode else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        let is_control = c.is_control();
        match (mode, is_control) {
            (_, false) => {
                out.push(c);
                in_run = false;
            }
            (ControlChars::Remove, true) => {}
            (ControlChars::SpacePerChar, true) => out.push(' '),
            (ControlChars::SpacePerRun, true) => {
                if !in_run {
                    out.push(' ');
                }
                in_run = true;
            }
        }
    }
    out
}

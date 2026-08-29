// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Accumulating input lines into batches and expanding `$(var)` references.

pub mod scanner;
pub mod substitute;

use crate::vars::Variables;
use scanner::{Scanner, ScannerState};

/// What a line of input turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum LineKind {
    /// Ordinary text; it has been appended to the statement cache.
    Buffered,
    /// A batch terminator. Run the cache `count` times.
    Terminator { count: u32 },
    /// A colon command, given verbatim without its leading colon.
    Command(String),
}

/// Splits input into lines, pairing each with its own terminator.
///
/// `str::lines` would throw the terminator away, but a string literal left open
/// across a line boundary carries it into the statement, so `SELECT 'a<CRLF>b'`
/// read from a CRLF file must reach the server with the `\r` intact.
pub fn split_lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.split_inclusive('\n').map(|line| {
        let content = line.trim_end_matches(['\r', '\n']);
        (content, &line[content.len()..])
    })
}

/// The statement cache plus the lexical state carried across its lines.
#[derive(Debug, Default)]
pub struct Batch {
    text: String,
    scanner: Scanner,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// True when the cache ends inside a string literal or block comment, in
    /// which case a terminator on the next line is just more literal text.
    pub fn is_open(&self) -> bool {
        self.scanner.state() != ScannerState::Normal
    }

    pub fn reset(&mut self) {
        self.text.clear();
        self.scanner = Scanner::default();
    }

    pub fn line_count(&self) -> usize {
        if self.text.is_empty() {
            0
        } else {
            self.text.lines().count()
        }
    }

    /// Classifies one line of input and, when it is ordinary text, appends it.
    ///
    /// `terminator` is the batch terminator in force (`GO` unless `-c` changed
    /// it). Colon commands and terminators are only recognised when the cache is
    /// not sitting inside a literal. `eol` is the line's own terminator, kept
    /// verbatim because a string literal spanning lines carries it to the server.
    pub fn push_line(&mut self, line: &str, eol: &str, terminator: &str) -> LineKind {
        if !self.is_open() {
            if let Some(count) = match_terminator(line, terminator) {
                return LineKind::Terminator { count };
            }
            if let Some(command) = match_command(line) {
                return LineKind::Command(command);
            }
        }

        self.push_text(line, eol);
        LineKind::Buffered
    }

    /// Appends a line that turned out not to be a command after all.
    pub fn push_text(&mut self, line: &str, eol: &str) {
        self.scanner.feed(line);
        self.text.push_str(line);
        self.text.push_str(eol);
    }

    /// Expands `$(var)` and hands back the text to send.
    pub fn resolve(&self, vars: &Variables, substitute: bool) -> substitute::Expansion {
        if substitute {
            substitute::expand(&self.text, vars)
        } else {
            substitute::Expansion {
                text: self.text.clone(),
                undefined: Vec::new(),
            }
        }
    }
}

/// A terminator occupies its own line, is matched case-insensitively, and may
/// carry a repeat count.
fn match_terminator(line: &str, terminator: &str) -> Option<u32> {
    let trimmed = line.trim();
    if trimmed.len() < terminator.len() {
        return None;
    }
    let (head, tail) = trimmed.split_at(terminator.len());
    if !head.eq_ignore_ascii_case(terminator) {
        return None;
    }

    let tail = tail.trim();
    if tail.is_empty() {
        return Some(1);
    }
    // `GO5` is not a terminator; `GO 5` is.
    if !tail.starts_with(char::is_whitespace) && !head.is_empty() {
        let following = trimmed[terminator.len()..].chars().next();
        if following.is_some_and(|c| !c.is_whitespace()) {
            return None;
        }
    }
    tail.parse::<u32>().ok()
}

/// Colon commands start at the very beginning of the line. `GO` is handled by
/// [`match_terminator`] because it has no colon.
fn match_command(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix(':')?;
    // `::` is not a command, and neither is a bare colon.
    if rest.starts_with(':') || rest.trim().is_empty() {
        return None;
    }
    Some(rest.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(batch: &mut Batch, line: &str) -> LineKind {
        batch.push_line(line, "\n", "GO")
    }

    #[test]
    fn go_on_its_own_line_terminates_the_batch() {
        let mut batch = Batch::default();
        assert_eq!(push(&mut batch, "SELECT 1"), LineKind::Buffered);
        assert_eq!(push(&mut batch, "GO"), LineKind::Terminator { count: 1 });
        assert_eq!(batch.text(), "SELECT 1\n");
    }

    #[test]
    fn go_is_case_insensitive_and_may_be_indented() {
        let mut batch = Batch::default();
        assert_eq!(
            push(&mut batch, "  go  "),
            LineKind::Terminator { count: 1 }
        );
    }

    #[test]
    fn go_takes_a_repeat_count() {
        let mut batch = Batch::default();
        assert_eq!(push(&mut batch, "GO 3"), LineKind::Terminator { count: 3 });
    }

    #[test]
    fn go_glued_to_something_else_is_not_a_terminator() {
        let mut batch = Batch::default();
        assert_eq!(push(&mut batch, "GOTO x"), LineKind::Buffered);
        assert_eq!(push(&mut batch, "GO5"), LineKind::Buffered);
    }

    #[test]
    fn a_custom_terminator_replaces_go() {
        let mut batch = Batch::default();
        assert_eq!(
            batch.push_line("END", "\n", "END"),
            LineKind::Terminator { count: 1 }
        );
        assert_eq!(batch.push_line("GO", "\n", "END"), LineKind::Buffered);
    }

    #[test]
    fn a_terminator_inside_a_string_literal_is_just_text() {
        let mut batch = Batch::default();
        push(&mut batch, "SELECT '");
        assert_eq!(push(&mut batch, "GO"), LineKind::Buffered);
        assert_eq!(push(&mut batch, "' AS a"), LineKind::Buffered);
        assert_eq!(push(&mut batch, "GO"), LineKind::Terminator { count: 1 });
    }

    #[test]
    fn a_terminator_inside_a_block_comment_is_just_text() {
        let mut batch = Batch::default();
        push(&mut batch, "/*");
        assert_eq!(push(&mut batch, "GO"), LineKind::Buffered);
        push(&mut batch, "*/");
        assert_eq!(push(&mut batch, "GO"), LineKind::Terminator { count: 1 });
    }

    #[test]
    fn colon_commands_are_recognised_at_the_start_of_a_line() {
        let mut batch = Batch::default();
        assert_eq!(
            push(&mut batch, ":setvar A 1"),
            LineKind::Command("setvar A 1".into())
        );
        assert_eq!(push(&mut batch, "SELECT ':r x'"), LineKind::Buffered);
    }

    #[test]
    fn reset_clears_both_text_and_lexical_state() {
        let mut batch = Batch::default();
        push(&mut batch, "SELECT '");
        assert!(batch.is_open());
        batch.reset();
        assert!(!batch.is_open());
        assert!(batch.is_empty());
    }
}

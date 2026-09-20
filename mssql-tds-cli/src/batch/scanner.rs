// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tracks whether the statement cache currently sits inside a literal or comment.
//!
//! Only the states that survive a line break matter here: a string literal, a
//! quoted identifier and a block comment can all span lines, so a `GO` found
//! while one is open is ordinary text rather than a batch terminator. Line
//! comments end at the newline and so never carry over.

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ScannerState {
    #[default]
    Normal,
    /// Inside `'...'`.
    String,
    /// Inside `"..."`.
    QuotedIdentifier,
    /// Inside `[...]`.
    Bracket,
    /// Inside `/* ... */`, with the current nesting depth.
    BlockComment(u32),
}

#[derive(Debug, Default, Clone)]
pub struct Scanner {
    state: ScannerState,
}

impl Scanner {
    pub fn state(&self) -> ScannerState {
        self.state
    }

    /// Advances the state across one line of input.
    pub fn feed(&mut self, line: &str) {
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            match self.state {
                ScannerState::Normal => match (c, next) {
                    ('-', Some('-')) => return, // line comment runs to end of line
                    ('/', Some('*')) => {
                        self.state = ScannerState::BlockComment(1);
                        i += 1;
                    }
                    ('\'', _) => self.state = ScannerState::String,
                    ('"', _) => self.state = ScannerState::QuotedIdentifier,
                    ('[', _) => self.state = ScannerState::Bracket,
                    _ => {}
                },
                // `''` is an escaped quote, not a close followed by an open.
                ScannerState::String => {
                    if c == '\'' {
                        if next == Some('\'') {
                            i += 1;
                        } else {
                            self.state = ScannerState::Normal;
                        }
                    }
                }
                ScannerState::QuotedIdentifier => {
                    if c == '"' {
                        if next == Some('"') {
                            i += 1;
                        } else {
                            self.state = ScannerState::Normal;
                        }
                    }
                }
                ScannerState::Bracket => {
                    if c == ']' {
                        if next == Some(']') {
                            i += 1;
                        } else {
                            self.state = ScannerState::Normal;
                        }
                    }
                }
                // T-SQL block comments nest.
                ScannerState::BlockComment(depth) => match (c, next) {
                    ('/', Some('*')) => {
                        self.state = ScannerState::BlockComment(depth + 1);
                        i += 1;
                    }
                    ('*', Some('/')) => {
                        self.state = if depth <= 1 {
                            ScannerState::Normal
                        } else {
                            ScannerState::BlockComment(depth - 1)
                        };
                        i += 1;
                    }
                    _ => {}
                },
            }
            i += 1;
        }
    }
}

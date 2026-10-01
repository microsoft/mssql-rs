// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Which tool's behaviour to imitate where the two disagree.
//!
//! ODBC sqlcmd and go-sqlcmd differ in ways that cannot both be satisfied — the
//! row-count wording, several column widths, and how floats and GUIDs are
//! rendered. ODBC is the default because it is the older and more widely
//! scripted-against tool; `--compat go` switches the differences over, and the
//! `compat-go` build feature makes that the default instead.
//!
//! Every difference encoded here was measured by running both binaries against
//! the same local SQL Server, not read from either's documentation.

/// The behaviour to follow where the two tools disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    Odbc,
    Go,
}

impl Default for Compat {
    fn default() -> Self {
        if cfg!(feature = "compat-go") {
            Compat::Go
        } else {
            Compat::Odbc
        }
    }
}

impl Compat {
    /// Parses a `--compat` value or `SQLCMDCOMPAT`. Returns `None` for a name
    /// neither tool answers to, so the caller can refuse rather than guess.
    /// Precedence between the two sources is settled in `cli::validate`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "odbc" => Some(Compat::Odbc),
            "go" | "go-sqlcmd" => Some(Compat::Go),
            _ => None,
        }
    }

    pub fn is_go(self) -> bool {
        self == Compat::Go
    }
}

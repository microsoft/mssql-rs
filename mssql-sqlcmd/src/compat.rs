// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Which tool's behaviour to imitate where the two disagree.
//!
//! ODBC sqlcmd and go-sqlcmd differ in ways that cannot both be satisfied — the
//! row-count wording, several column widths, and how floats and GUIDs are
//! rendered. ODBC is the default because it is the older and more widely
//! scripted-against tool; `--compat go` switches the differences over.
//!
//! Every difference encoded here was measured by running both binaries against
//! the same local SQL Server, not read from either's documentation.

/// The behaviour to follow where the two tools disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compat {
    #[default]
    Odbc,
    Go,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_tool_names_are_recognised() {
        assert_eq!(Compat::parse("odbc"), Some(Compat::Odbc));
        assert_eq!(Compat::parse("ODBC"), Some(Compat::Odbc));
        assert_eq!(Compat::parse("go"), Some(Compat::Go));
        assert_eq!(Compat::parse(" go-sqlcmd "), Some(Compat::Go));
    }

    #[test]
    fn an_unknown_name_is_refused_rather_than_defaulted() {
        assert_eq!(Compat::parse("nonsense"), None);
        assert_eq!(Compat::parse(""), None);
    }

    #[test]
    fn odbc_is_the_default() {
        assert_eq!(Compat::default(), Compat::Odbc);
        assert!(!Compat::default().is_go());
    }
}

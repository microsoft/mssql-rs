// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The option table.
//!
//! Short names, arities and value shapes follow ODBC sqlcmd, which is the
//! compatibility target. Where go-sqlcmd exposes a long name for the same
//! option it is listed as an alias so scripts written for either tool work.

/// How many, and what shape of, tokens an option consumes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arity {
    /// Takes nothing. Anything attached to the flag is an error.
    Flag,
    /// Takes a value, either attached (`-Sfoo`) or as the following token (`-S foo`).
    Value,
    /// Takes an optional single-character suffix, attached only (`-Ns`, `-r0`).
    Suffix(&'static [char]),
    /// Accepted but ignored, with a warning.
    Retired,
}

pub struct Spec {
    /// The ODBC short letter. Options that go-sqlcmd added have no short form
    /// and use a private-use marker here so the rest of the parser can keep
    /// treating the short name as a key; [`by_short`] refuses to match them.
    pub short: char,
    pub long: Option<&'static str>,
    pub arity: Arity,
}

/// Markers for the long-only options. Private-use scalars cannot be typed as
/// `-x`, so they can never be reached by a short-form lookup.
pub const VERTICAL: char = '\u{E000}';
pub const ASCII: char = '\u{E001}';
pub const FORMAT: char = '\u{E002}';
pub const VERSION: char = '\u{E003}';
pub const AUTH_METHOD: char = '\u{E004}';
pub const SERVER_NAME: char = '\u{E005}';
pub const DRIVER_LOGGING: char = '\u{E006}';
pub const TRACE_FILE: char = '\u{E007}';
pub const COMPAT: char = '\u{E008}';

/// Whether a short name is one a user could actually type.
fn is_typeable(short: char) -> bool {
    short.is_ascii_alphanumeric() || short == '?'
}

const fn spec(short: char, long: Option<&'static str>, arity: Arity) -> Spec {
    Spec { short, long, arity }
}

pub const SPECS: &[Spec] = &[
    spec('a', Some("packet-size"), Arity::Value),
    spec('A', Some("dedicated-admin-connection"), Arity::Flag),
    spec('b', Some("exit-on-error"), Arity::Flag),
    spec('c', Some("batch-terminator"), Arity::Value),
    spec('C', Some("trust-server-certificate"), Arity::Flag),
    spec('d', Some("database-name"), Arity::Value),
    spec('D', Some("dsn"), Arity::Value),
    spec('e', Some("echo-input"), Arity::Flag),
    spec('E', Some("use-trusted-connection"), Arity::Flag),
    spec('f', None, Arity::Value),
    spec('F', Some("host-name-in-certificate"), Arity::Value),
    spec('g', Some("enable-column-encryption"), Arity::Flag),
    spec('G', Some("use-aad"), Arity::Flag),
    spec('h', Some("headers"), Arity::Value),
    spec('H', Some("workstation-name"), Arity::Value),
    spec('i', Some("input-file"), Arity::Value),
    spec('I', Some("enable-quoted-identifiers"), Arity::Flag),
    spec('j', Some("raw-errors"), Arity::Flag),
    spec('J', Some("server-certificate"), Arity::Value),
    spec(
        'k',
        Some("remove-control-characters"),
        Arity::Suffix(&['1', '2']),
    ),
    spec('K', Some("application-intent"), Arity::Value),
    spec('l', Some("login-timeout"), Arity::Value),
    spec('L', Some("list-servers"), Arity::Suffix(&['c'])),
    spec('m', Some("error-level"), Arity::Value),
    spec('M', Some("multi-subnet-failover"), Arity::Flag),
    spec('n', None, Arity::Retired),
    spec(
        'N',
        Some("encrypt-connection"),
        Arity::Suffix(&['s', 'm', 'o']),
    ),
    spec('o', Some("output-file"), Arity::Value),
    spec('O', None, Arity::Retired),
    spec('p', Some("print-statistics"), Arity::Suffix(&['1'])),
    spec('P', Some("password"), Arity::Value),
    spec('q', Some("initial-query"), Arity::Value),
    spec('Q', Some("query"), Arity::Value),
    spec('r', Some("errors-to-stderr"), Arity::Suffix(&['0', '1'])),
    spec('R', Some("client-regional-setting"), Arity::Flag),
    spec('s', Some("column-separator"), Arity::Value),
    spec('S', Some("server"), Arity::Value),
    spec('t', Some("query-timeout"), Arity::Value),
    spec('T', None, Arity::Value),
    spec('u', Some("unicode-output-file"), Arity::Flag),
    spec('U', Some("user-name"), Arity::Value),
    spec('v', Some("variables"), Arity::Value),
    spec('V', Some("error-severity-level"), Arity::Value),
    spec('w', Some("screen-width"), Arity::Value),
    spec('W', Some("trim-spaces"), Arity::Flag),
    spec('x', Some("disable-variable-substitution"), Arity::Flag),
    spec('X', Some("disable-cmd-and-warn"), Arity::Suffix(&['1'])),
    spec('y', Some("variable-type-width"), Arity::Value),
    spec('Y', Some("fixed-type-width"), Arity::Value),
    spec('z', Some("change-password"), Arity::Value),
    spec('Z', Some("change-password-exit"), Arity::Value),
    spec('?', Some("help"), Arity::Flag),
    // Added by go-sqlcmd; long form only.
    spec(VERTICAL, Some("vertical"), Arity::Flag),
    spec(ASCII, Some("ascii"), Arity::Flag),
    spec(FORMAT, Some("format"), Arity::Value),
    spec(VERSION, Some("version"), Arity::Flag),
    spec(AUTH_METHOD, Some("authentication-method"), Arity::Value),
    spec(SERVER_NAME, Some("server-name"), Arity::Value),
    spec(DRIVER_LOGGING, Some("driver-logging-level"), Arity::Value),
    spec(TRACE_FILE, Some("trace-file"), Arity::Value),
    // Ours: picks whose behaviour to follow where the two tools disagree.
    spec(COMPAT, Some("compat"), Arity::Value),
];

pub fn by_short(c: char) -> Option<&'static Spec> {
    if !is_typeable(c) {
        return None;
    }
    SPECS.iter().find(|s| s.short == c)
}

/// Whether `name` is a long option with no short form a user could type.
///
/// These are the options go-sqlcmd added, so they are exactly the ones the
/// ODBC tool has no spelling for. When this implementation is linked into the
/// native binary, that makes them safe to claim: nothing written against ODBC
/// `sqlcmd` can be using them.
pub fn is_long_only(name: &str) -> bool {
    by_long(name).is_some_and(|s| !is_typeable(s.short))
}

/// Long names are matched without regard to case.
///
/// go-sqlcmd's parser is case-sensitive and it spells one flag `--login-timeOut`
/// with a capital `O`, so a script written against it would otherwise be
/// rejected here. Accepting either spelling costs nothing: no two options in
/// the table differ only by case.
pub fn by_long(name: &str) -> Option<&'static Spec> {
    SPECS
        .iter()
        .find(|s| s.long.is_some_and(|long| long.eq_ignore_ascii_case(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_are_unique() {
        let mut seen: Vec<char> = SPECS.iter().map(|s| s.short).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "duplicate short option in SPECS");
    }

    #[test]
    fn long_names_are_unique() {
        // Compared case-insensitively, because that is how `by_long` matches:
        // two options differing only by case would make the lookup ambiguous.
        let mut seen: Vec<String> = SPECS
            .iter()
            .filter_map(|s| s.long)
            .map(str::to_ascii_lowercase)
            .collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "duplicate long option in SPECS");
    }

    #[test]
    fn long_names_ignore_case_so_the_go_spelling_is_accepted() {
        // go-sqlcmd spells this one with a capital `O`.
        assert!(by_long("login-timeOut").is_some());
        assert!(by_long("login-timeout").is_some());
        assert!(by_long("VERTICAL").is_some());
        assert!(by_long("no-such-option").is_none());
    }

    #[test]
    fn every_usage_option_has_a_spec() {
        for c in "UPSHENCFdlthswaeIcLqQmVWurioz fZkyYpRKMbvAXxjgG?".chars() {
            if c == ' ' {
                continue;
            }
            assert!(
                by_short(c).is_some(),
                "-{c} appears in usage but has no spec"
            );
        }
    }
}

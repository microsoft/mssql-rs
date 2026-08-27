// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line lexing.
//!
//! ODBC sqlcmd does not use a getopt-style parser: `-` and `/` are both option
//! prefixes, values may be attached or separate, a handful of options take an
//! attached single-character suffix instead of a value, and a token like `-1`
//! is a number rather than an option. This module reproduces that grammar and
//! nothing more; range checks and conflict rules live in [`super::validate`].

use crate::messages;

use super::spec::{Arity, Spec, by_long, by_short};

/// A parse failure, carrying the stream the reference writes it to.
///
/// The reference is not consistent about this — most diagnostics go to stderr,
/// but `MSG_MULTIPLE_SAME_OPT` goes to stdout — so the stream is part of the
/// error rather than a property of the call site.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Stdout(String),
    #[error("{0}")]
    Stderr(String),
}

impl CliError {
    pub fn stream_is_stdout(&self) -> bool {
        matches!(self, CliError::Stdout(_))
    }
}

/// One option as it appeared on the command line, before any interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawOption {
    pub short: char,
    /// The value for [`Arity::Value`] options.
    pub value: Option<String>,
    /// The attached character for [`Arity::Suffix`] options.
    pub suffix: Option<char>,
}

/// The result of lexing: the options in the order they were written, plus any
/// warnings to emit before acting on them.
#[derive(Debug, Default)]
pub struct Lexed {
    pub options: Vec<RawOption>,
    pub warnings: Vec<String>,
}

impl Lexed {
    pub fn contains(&self, short: char) -> bool {
        self.options.iter().any(|o| o.short == short)
    }

    #[cfg(test)]
    pub fn first(&self, short: char) -> Option<&RawOption> {
        self.options.iter().find(|o| o.short == short)
    }

    pub fn count(&self, short: char) -> usize {
        self.options.iter().filter(|o| o.short == short).count()
    }
}

/// Whether `c` introduces an option.
///
/// `/` is a Windows convention; the Unix reference treats `/?` as an unknown
/// option rather than a request for help, since a leading slash there is a path.
fn is_prefix(c: char) -> bool {
    c == '-' || (cfg!(windows) && c == '/')
}

/// Whether `token` should be read as the next option rather than as the
/// pending option's value.
///
/// `-h -1` must treat `-1` as a value, so a prefix followed by a digit is a
/// number. `-S -Q` must report a missing argument, so a prefix followed by a
/// known option letter is an option.
///
/// The Unix reference makes no such distinction — a value option swallows
/// whatever comes next, and the token after that is then reported as an unknown
/// option.
fn starts_new_option(token: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(p), Some(c)) if is_prefix(p) => !c.is_ascii_digit() && by_short(c).is_some(),
        _ => false,
    }
}

fn lex_long(spec: &Spec, inline: Option<&str>) -> Result<RawOption, CliError> {
    match spec.arity {
        Arity::Flag | Arity::Retired => match inline {
            Some(v) => Err(CliError::Stderr(messages::unexpected_arg(v))),
            None => Ok(RawOption {
                short: spec.short,
                value: None,
                suffix: None,
            }),
        },
        Arity::Value => match inline {
            Some(v) => Ok(RawOption {
                short: spec.short,
                value: Some(v.to_string()),
                suffix: None,
            }),
            // A separate value is handled by the caller, which owns the token
            // cursor; signal that by leaving the value empty.
            None => Ok(RawOption {
                short: spec.short,
                value: None,
                suffix: None,
            }),
        },
        Arity::Suffix(allowed) => match inline {
            None => Ok(RawOption {
                short: spec.short,
                value: None,
                suffix: None,
            }),
            Some(v) => {
                let mut chars = v.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if allowed.contains(&c) => Ok(RawOption {
                        short: spec.short,
                        value: None,
                        suffix: Some(c),
                    }),
                    _ => Err(CliError::Stderr(messages::invalid_parameters(spec.short))),
                }
            }
        },
    }
}

/// Split the command line into options.
pub fn lex(argv: &[String]) -> Result<Lexed, CliError> {
    let mut out = Lexed::default();
    let mut i = 0;

    while i < argv.len() {
        let token = &argv[i];
        i += 1;

        let Some(prefix) = token.chars().next() else {
            return Err(CliError::Stderr(messages::unexpected_arg(token)));
        };

        if let Some(name) = token.strip_prefix("--") {
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (name, None),
            };
            let Some(spec) = by_long(name) else {
                return Err(CliError::Stderr(messages::unknown_option(name)));
            };
            let mut opt = lex_long(spec, inline)?;
            if spec.arity == Arity::Value && opt.value.is_none() {
                match argv.get(i) {
                    Some(next) if !starts_new_option(next) => {
                        opt.value = Some(next.clone());
                        i += 1;
                    }
                    _ => return Err(CliError::Stderr(messages::missing_arg(spec.short))),
                }
            }
            if spec.arity == Arity::Retired {
                out.warnings.push(messages::retired_option(spec.short));
            } else {
                out.options.push(opt);
            }
            continue;
        }

        if !is_prefix(prefix) {
            // A lone `/` is still reported as a prefix without an argument,
            // even where `/` no longer introduces an option.
            if token == "/" {
                return Err(CliError::Stderr(messages::argument_missing()));
            }
            // Windows calls a stray token an unexpected argument; Unix calls it
            // an unknown option.
            return Err(CliError::Stderr(if cfg!(windows) {
                messages::unexpected_arg(token)
            } else {
                messages::unknown_option(token)
            }));
        }

        let rest: String = token.chars().skip(1).collect();
        let Some(short) = rest.chars().next() else {
            return Err(CliError::Stderr(messages::argument_missing()));
        };

        // `-9` is a number, not an option. Windows reports it with its prefix
        // intact, unlike an unknown letter; Unix strips the prefix from both.
        if short.is_ascii_digit() {
            let shown = if cfg!(windows) { token.as_str() } else { &rest };
            return Err(CliError::Stderr(messages::unknown_option(shown)));
        }

        let Some(spec) = by_short(short) else {
            return Err(CliError::Stderr(messages::unknown_option(&rest)));
        };

        let attached: String = rest.chars().skip(1).collect();

        match spec.arity {
            Arity::Retired => {
                if !attached.is_empty() {
                    return Err(CliError::Stderr(messages::unexpected_arg(&attached)));
                }
                out.warnings.push(messages::retired_option(short));
            }
            Arity::Flag => {
                if !attached.is_empty() {
                    return Err(CliError::Stderr(messages::unexpected_arg(&attached)));
                }
                out.options.push(RawOption {
                    short,
                    value: None,
                    suffix: None,
                });
            }
            Arity::Suffix(allowed) => {
                let suffix = if attached.is_empty() {
                    None
                } else {
                    let mut chars = attached.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) if allowed.contains(&c) => Some(c),
                        _ => return Err(CliError::Stderr(messages::invalid_parameters(short))),
                    }
                };
                out.options.push(RawOption {
                    short,
                    value: None,
                    suffix,
                });
            }
            Arity::Value => {
                let value = if attached.is_empty() {
                    match argv.get(i) {
                        Some(next) if !starts_new_option(next) => {
                            i += 1;
                            next.clone()
                        }
                        _ => return Err(CliError::Stderr(messages::missing_arg(short))),
                    }
                } else {
                    attached
                };
                out.options.push(RawOption {
                    short,
                    value: Some(value),
                    suffix: None,
                });
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::EOL;

    fn lex_ok(args: &[&str]) -> Lexed {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        lex(&argv).expect("expected a successful lex")
    }

    fn lex_err(args: &[&str]) -> String {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        lex(&argv).expect_err("expected a lex failure").to_string()
    }

    #[test]
    fn separate_and_attached_values_are_equivalent() {
        let separate = lex_ok(&["-S", "myserver"]);
        let attached = lex_ok(&["-Smyserver"]);
        assert_eq!(separate.options, attached.options);
        assert_eq!(
            separate.first('S').unwrap().value.as_deref(),
            Some("myserver")
        );
    }

    #[test]
    fn slash_is_an_option_prefix_only_on_windows() {
        if cfg!(windows) {
            assert!(lex_ok(&["/?"]).contains('?'));
        } else {
            // A leading slash is a path on Unix, and the reference says so.
            assert_eq!(
                lex_err(&["/?"]),
                format!("Sqlcmd: '/?': Unknown Option. Enter '-?' for help.{EOL}")
            );
        }
    }

    #[test]
    fn a_lone_slash_is_a_prefix_without_an_argument_everywhere() {
        assert_eq!(
            lex_err(&["/"]),
            format!(
                "Sqlcmd: Error: '-' or '/' does not have an associated argument.{EOL}Enter '-?' for help.{EOL}"
            )
        );
    }

    #[test]
    fn negative_number_is_a_value_not_an_option() {
        let lexed = lex_ok(&["-h", "-1"]);
        assert_eq!(lexed.first('h').unwrap().value.as_deref(), Some("-1"));
    }

    #[test]
    fn value_option_followed_by_an_option() {
        if cfg!(windows) {
            assert_eq!(
                lex_err(&["-S", "-Q", "select 1"]),
                format!("Sqlcmd: '-S': Missing argument. Enter '-?' for help.{EOL}")
            );
        } else {
            // Unix takes whatever follows as the value, so the complaint lands
            // on the token after it.
            assert_eq!(
                lex_err(&["-S", "-Q", "select 1"]),
                format!("Sqlcmd: 'select 1': Unknown Option. Enter '-?' for help.{EOL}")
            );
        }
    }

    #[test]
    fn trailing_value_option_is_a_missing_argument() {
        assert_eq!(
            lex_err(&["-S"]),
            format!("Sqlcmd: '-S': Missing argument. Enter '-?' for help.{EOL}")
        );
    }

    #[test]
    fn unknown_letter_is_reported_without_its_prefix() {
        assert_eq!(
            lex_err(&["-BOGUS"]),
            format!("Sqlcmd: 'BOGUS': Unknown Option. Enter '-?' for help.{EOL}")
        );
    }

    #[test]
    fn unknown_digit_keeps_its_prefix_only_on_windows() {
        let expected = if cfg!(windows) {
            format!("Sqlcmd: '-9': Unknown Option. Enter '-?' for help.{EOL}")
        } else {
            format!("Sqlcmd: '9': Unknown Option. Enter '-?' for help.{EOL}")
        };
        assert_eq!(lex_err(&["-9"]), expected);
    }

    #[test]
    fn junk_attached_to_a_flag_is_an_unexpected_argument() {
        assert_eq!(
            lex_err(&["-eXYZ"]),
            format!("Sqlcmd: 'XYZ': Unexpected argument. Enter '-?' for help.{EOL}")
        );
    }

    #[test]
    fn bare_prefix_has_no_associated_argument() {
        let expected = format!(
            "Sqlcmd: Error: '-' or '/' does not have an associated argument.{EOL}Enter '-?' for help.{EOL}"
        );
        assert_eq!(lex_err(&["-"]), expected);
        assert_eq!(lex_err(&["/"]), expected);
    }

    #[test]
    fn suffix_options_accept_only_their_own_alphabet() {
        assert_eq!(lex_ok(&["-Ns"]).first('N').unwrap().suffix, Some('s'));
        assert_eq!(lex_ok(&["-N"]).first('N').unwrap().suffix, None);
        assert_eq!(
            lex_err(&["-Nx"]),
            format!("Sqlcmd: Command -N: Invalid Parameters passed.{EOL}")
        );
    }

    #[test]
    fn suffix_options_do_not_consume_the_next_token() {
        let lexed = lex_ok(&["-N", "-Q", "select 1"]);
        assert_eq!(lexed.first('N').unwrap().suffix, None);
        assert_eq!(lexed.first('Q').unwrap().value.as_deref(), Some("select 1"));
    }

    #[test]
    fn go_long_names_map_onto_odbc_short_names() {
        let long = lex_ok(&["--server", "myserver", "--trim-spaces"]);
        let short = lex_ok(&["-S", "myserver", "-W"]);
        assert_eq!(long.options, short.options);
    }

    #[test]
    fn long_names_accept_an_equals_form() {
        assert_eq!(
            lex_ok(&["--server=myserver"])
                .first('S')
                .unwrap()
                .value
                .as_deref(),
            Some("myserver")
        );
    }

    #[test]
    fn retired_options_warn_and_are_dropped() {
        let lexed = lex_ok(&["-n"]);
        assert!(lexed.options.is_empty());
        assert_eq!(lexed.warnings.len(), 1);
    }

    #[test]
    fn repeated_options_are_all_retained_in_order() {
        let lexed = lex_ok(&["-i", "a.sql", "-i", "b.sql"]);
        assert_eq!(lexed.count('i'), 2);
        assert_eq!(lexed.options[1].value.as_deref(), Some("b.sql"));
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! go-sqlcmd's subcommand CLI, alongside the legacy flag-driven one.
//!
//! go-sqlcmd grew a second, verb-based interface — `sqlcmd config add-context`,
//! `sqlcmd create mssql`, `sqlcmd query` — while keeping the original. The two
//! are told apart by the first argument alone: if it names a subcommand the new
//! CLI runs, otherwise everything falls through to the flag parser.
//!
//! That rule is the whole of the dispatch, and it means a script calling
//! `sqlcmd -Q "..."` is untouched.

pub mod config_cmds;
pub mod container;
pub mod server_cmds;
pub mod sqlconfig;
pub mod yaml;

use std::path::PathBuf;

/// The first words that mean "this is a subcommand, not a flag".
const SUBCOMMANDS: &[&str] = &[
    "config",
    "create",
    "install",
    "delete",
    "uninstall",
    "drop",
    "remove",
    "query",
    "start",
    "stop",
    "open",
];

/// Whether `argv` opens with a subcommand.
pub fn claims(argv: &[String]) -> bool {
    argv.first().is_some_and(|first| {
        SUBCOMMANDS.contains(&first.as_str()) || first == "--help" && argv.len() == 1
    })
}

/// Options common to every subcommand.
pub struct Context {
    pub path: PathBuf,
    pub config: sqlconfig::SqlConfig,
}

impl Context {
    fn open(path: PathBuf) -> Result<Self, String> {
        let config = sqlconfig::SqlConfig::load(&path)?;
        Ok(Context { path, config })
    }

    pub fn save(&self) -> Result<(), String> {
        self.config.save(&self.path)
    }
}

/// A parsed subcommand invocation: the words naming it, plus its flags.
pub struct Invocation {
    pub words: Vec<String>,
    flags: Vec<(String, Option<String>)>,
    pub positional: Vec<String>,
}

impl Invocation {
    /// Splits `argv` into leading words, `--flag[=value]` pairs and positionals.
    ///
    /// A flag's value may be attached with `=` or be the next argument, which
    /// is how the Go flag library behaves. Booleans take no value, so they are
    /// named here rather than guessed at.
    fn parse(argv: &[String], booleans: &[&str]) -> Result<Self, String> {
        let mut words = Vec::new();
        let mut flags = Vec::new();
        let mut positional = Vec::new();
        let mut index = 0;
        let mut seen_flag = false;

        while index < argv.len() {
            let argument = &argv[index];
            if let Some(name) = argument.strip_prefix("--") {
                seen_flag = true;
                if let Some((name, value)) = name.split_once('=') {
                    flags.push((name.to_string(), Some(value.to_string())));
                } else if booleans.contains(&name) {
                    flags.push((name.to_string(), None));
                } else {
                    index += 1;
                    let value = argv
                        .get(index)
                        .ok_or_else(|| format!("flag needs an argument: --{name}"))?;
                    flags.push((name.to_string(), Some(value.clone())));
                }
            } else if argument.starts_with('-') && argument.len() > 1 {
                return Err(format!("unknown shorthand flag in {argument:?}"));
            } else if seen_flag {
                positional.push(argument.clone());
            } else {
                words.push(argument.clone());
            }
            index += 1;
        }

        // Words after the command name are positional arguments; how many words
        // name the command is decided by the caller via `take_word`.
        Ok(Invocation {
            words,
            flags,
            positional,
        })
    }

    /// Removes and returns the leading word, if any.
    pub fn take_word(&mut self) -> Option<String> {
        if self.words.is_empty() {
            None
        } else {
            Some(self.words.remove(0))
        }
    }

    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_deref().unwrap_or(""))
    }

    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }

    pub fn flag_or<'a>(&'a self, name: &str, fallback: &'a str) -> &'a str {
        self.flag(name).unwrap_or(fallback)
    }

    pub fn number(&self, name: &str, fallback: u16) -> Result<u16, String> {
        match self.flag(name) {
            None => Ok(fallback),
            Some(text) => text
                .parse()
                .map_err(|_| format!("invalid argument {text:?} for --{name}")),
        }
    }

    /// A name given either as `--name x` or as the first positional argument,
    /// which every `get-*` and `delete-*` command accepts interchangeably.
    pub fn name_argument(&mut self) -> Option<String> {
        self.flag("name")
            .map(str::to_string)
            .or_else(|| self.take_word())
            .or_else(|| self.positional.first().cloned())
    }
}

/// Flags that take no value, across every subcommand.
const BOOLEANS: &[&str] = &[
    "detailed",
    "raw",
    "cascade",
    "yes",
    "force",
    "accept-eula",
    "cached",
    "help",
    "version",
];

/// What a subcommand produced.
pub enum Outcome {
    /// Text to print, and the process is done.
    Done(String),
    /// `query` resolves the current context into legacy arguments and asks to
    /// be re-entered through the ordinary session machinery.
    Delegate(Vec<String>),
}

/// Runs a subcommand. The caller has already established that `argv` opens with
/// one. `Err` carries the message to print on stderr.
pub fn run(argv: &[String]) -> Result<Outcome, String> {
    dispatch(argv)
}

fn dispatch(argv: &[String]) -> Result<Outcome, String> {
    let mut invocation = Invocation::parse(argv, BOOLEANS)?;

    let path = invocation
        .flag("sqlconfig")
        .map(PathBuf::from)
        .unwrap_or_else(sqlconfig::default_path);

    let command = invocation.take_word().unwrap_or_default();
    if invocation.has("help") {
        return Ok(Outcome::Done(help_for(&command)));
    }

    match command.as_str() {
        "config" => config_cmds::run(&mut invocation, Context::open(path)?).map(Outcome::Done),
        "query" => server_cmds::query(&mut invocation, Context::open(path)?),
        "create" | "install" => {
            server_cmds::create(&mut invocation, Context::open(path)?).map(Outcome::Done)
        }
        "start" => server_cmds::start(Context::open(path)?).map(Outcome::Done),
        "stop" => server_cmds::stop(Context::open(path)?).map(Outcome::Done),
        "delete" | "uninstall" | "drop" | "remove" => {
            server_cmds::delete(&mut invocation, Context::open(path)?).map(Outcome::Done)
        }
        "open" => Err(unsupported("open")),
        "--help" => Ok(Outcome::Done(help_for(""))),
        other => Err(format!("unknown command {other:?}")),
    }
}

/// Commands that are recognised but not implemented say so, rather than
/// failing as though the name were a typo.
fn unsupported(name: &str) -> String {
    format!(
        "sqlcmd: '{name}' is not implemented in this build.\n\
         It launches an external application, which this port does not do."
    )
}

fn help_for(command: &str) -> String {
    match command {
        "config" => config_cmds::HELP.to_string(),
        _ => HELP.to_string(),
    }
}

const HELP: &str = "\
sqlcmd: Install/Create/Query SQL Server, Azure SQL, and Tools

Usage:
  sqlcmd [command]

Available Commands:
  config      Modify sqlconfig files using subcommands like \"sqlcmd config use-context mssql\"
  create      Install/Create SQL Server, Azure SQL, and Tools
  delete      Uninstall/Delete the current context
  query       Run a query against the current context
  start       Start current context
  stop        Stop current context

Flags:
      --sqlconfig string   Configuration file
      --help               help for sqlcmd

Use \"sqlcmd [command] --help\" for more information about a command.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn a_leading_subcommand_claims_the_command_line() {
        assert!(claims(&argv(&["config", "get-contexts"])));
        assert!(claims(&argv(&["query"])));
        assert!(claims(&argv(&["create", "mssql"])));
    }

    #[test]
    fn legacy_flags_are_left_to_the_old_parser() {
        assert!(!claims(&argv(&["-Q", "SELECT 1"])));
        assert!(!claims(&argv(&["-S", "localhost"])));
        assert!(!claims(&argv(&[])));
        // A file happening to be named `query` is still not a subcommand here,
        // because the legacy CLI takes no bare positional arguments.
        assert!(!claims(&argv(&["-i", "query"])));
    }

    #[test]
    fn flags_take_their_value_attached_or_separate() {
        let invocation =
            Invocation::parse(&argv(&["--name=a", "--address", "b"]), BOOLEANS).unwrap();
        assert_eq!(invocation.flag("name"), Some("a"));
        assert_eq!(invocation.flag("address"), Some("b"));
    }

    #[test]
    fn a_boolean_flag_consumes_nothing() {
        let mut invocation =
            Invocation::parse(&argv(&["get-contexts", "--detailed"]), BOOLEANS).unwrap();
        assert!(invocation.has("detailed"));
        assert_eq!(invocation.take_word().as_deref(), Some("get-contexts"));
    }

    #[test]
    fn a_flag_missing_its_value_is_refused() {
        assert!(Invocation::parse(&argv(&["--name"]), BOOLEANS).is_err());
    }

    #[test]
    fn a_name_may_be_a_flag_or_a_positional() {
        let mut by_flag = Invocation::parse(&argv(&["--name", "x"]), BOOLEANS).unwrap();
        assert_eq!(by_flag.name_argument().as_deref(), Some("x"));

        let mut by_position = Invocation::parse(&argv(&["x"]), BOOLEANS).unwrap();
        assert_eq!(by_position.name_argument().as_deref(), Some("x"));
    }

    #[test]
    fn a_non_numeric_port_is_refused() {
        let invocation = Invocation::parse(&argv(&["--port", "abc"]), BOOLEANS).unwrap();
        assert!(invocation.number("port", 1433).is_err());
        assert_eq!(invocation.number("absent", 1433).unwrap(), 1433);
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Colon commands.
//!
//! The set and their help text are taken from the reference's own `:help`.

/// A recognised command, already parsed into its arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Quit,
    /// `:exit`, `:exit()` or `:exit(query)`.
    Exit(ExitForm),
    List,
    /// `:list color` — the colour schemes `SQLCMDCOLORSCHEME` accepts, each
    /// shown against a sample statement.
    ListColor,
    ListVar,
    Reset,
    Error(String),
    Out(String),
    PerfTrace(String),
    SetVar {
        name: String,
        value: Option<String>,
    },
    Read(String),
    Connect(String),
    OnError(OnError),
    Editor,
    Shell(String),
    ServerList,
    Xml(bool),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExitForm {
    /// `:exit` — stop at once, without running the cache.
    Immediate,
    /// `:exit()` — run the cache, then stop with no value.
    RunCache,
    /// `:exit(query)` — run the query and use its first cell.
    Query(String),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum OnError {
    Exit,
    Ignore,
}

/// Why a line beginning with `:` was not accepted.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The word is not a command at all. The reference sends such a line to the
    /// server as ordinary text rather than complaining about it.
    NotACommand,
    BadArguments(String),
}

/// Splits the leading word from its argument.
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(char::is_whitespace) {
        Some(index) => (&text[..index], text[index..].trim()),
        None => (text, ""),
    }
}

pub fn parse(line: &str) -> Result<Command, ParseError> {
    // `!!` takes the rest of the line verbatim, including leading spaces.
    if let Some(rest) = line.strip_prefix("!!") {
        return Ok(Command::Shell(rest.trim().to_string()));
    }

    let (word, rest) = split_word(line);
    let lowered = word.to_ascii_lowercase();

    match lowered.as_str() {
        "help" => Ok(Command::Help),
        "quit" => Ok(Command::Quit),
        "list" => {
            // `:list color` is a separate command sharing the word.
            if rest.trim().eq_ignore_ascii_case("color") {
                Ok(Command::ListColor)
            } else {
                Ok(Command::List)
            }
        }
        "listvar" => Ok(Command::ListVar),
        "reset" => Ok(Command::Reset),
        "ed" => Ok(Command::Editor),
        "serverlist" => Ok(Command::ServerList),
        "error" => require_argument(word, rest).map(Command::Error),
        "out" => require_argument(word, rest).map(Command::Out),
        "perftrace" => require_argument(word, rest).map(Command::PerfTrace),
        "r" => require_argument(word, rest).map(Command::Read),
        "connect" => require_argument(word, rest).map(Command::Connect),
        "setvar" => parse_setvar(rest),
        "xml" => match rest.to_ascii_lowercase().as_str() {
            "on" => Ok(Command::Xml(true)),
            "off" => Ok(Command::Xml(false)),
            _ => Err(ParseError::BadArguments("xml".into())),
        },
        "on" => parse_on_error(rest),
        _ if lowered.starts_with("exit") => parse_exit(line.trim()),
        _ => Err(ParseError::NotACommand),
    }
}

fn require_argument(word: &str, rest: &str) -> Result<String, ParseError> {
    if rest.is_empty() {
        Err(ParseError::BadArguments(word.to_string()))
    } else {
        Ok(rest.to_string())
    }
}

fn parse_on_error(rest: &str) -> Result<Command, ParseError> {
    let (word, action) = split_word(rest);
    if !word.eq_ignore_ascii_case("error") {
        return Err(ParseError::NotACommand);
    }
    match action.to_ascii_lowercase().as_str() {
        "exit" => Ok(Command::OnError(OnError::Exit)),
        "ignore" => Ok(Command::OnError(OnError::Ignore)),
        _ => Err(ParseError::BadArguments("on error".into())),
    }
}

/// `:setvar NAME` with no value removes the variable.
fn parse_setvar(rest: &str) -> Result<Command, ParseError> {
    if rest.is_empty() {
        return Err(ParseError::BadArguments("setvar".into()));
    }
    let (name, value) = split_word(rest);
    let value = if value.is_empty() {
        None
    } else {
        Some(unquote(value))
    };
    Ok(Command::SetVar {
        name: name.to_string(),
        value,
    })
}

fn unquote(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        Some(inner) => inner.to_string(),
        None => trimmed.to_string(),
    }
}

fn parse_exit(text: &str) -> Result<Command, ParseError> {
    let rest = text[4..].trim();
    if rest.is_empty() {
        return Ok(Command::Exit(ExitForm::Immediate));
    }
    let inner = rest
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .ok_or_else(|| ParseError::BadArguments("exit".into()))?;
    if inner.trim().is_empty() {
        Ok(Command::Exit(ExitForm::RunCache))
    } else {
        Ok(Command::Exit(ExitForm::Query(inner.trim().to_string())))
    }
}

/// `MSG_CMD_HELP_TEXT`, copied from the reference's `:help`.
pub const HELP: &str = "\
:!! [<command>]
  - Executes a command in the Windows command shell.
:connect server[\\instance] [-l timeout] [-U user [-P password]]
  - Connects to a SQL Server instance.
:ed
  - Edits the current or last executed statement cache.
:error <dest>
  - Redirects error output to a file, stderr, or stdout.
:exit
  - Quits sqlcmd immediately.
:exit()
  - Execute statement cache; quit with no return value.
:exit(<query>)
  - Execute the specified query; returns numeric result.
go [<n>]
  - Executes the statement cache (n times).
:help
  - Shows this list of commands.
:list
  - Prints the content of the statement cache.
:listvar
  - Lists the set sqlcmd scripting variables.
:on error [exit|ignore]
  - Action for batch or sqlcmd command errors.
:out <filename>|stderr|stdout
  - Redirects query output to a file, stderr, or stdout.
:perftrace <filename>|stderr|stdout
  - Redirects timing output to a file, stderr, or stdout.
:quit
  - Quits sqlcmd immediately.
:r <filename>
  - Append file contents to the statement cache.
:reset
  - Discards the statement cache.
:serverlist
  - Lists the known local and network SQL Server instances.
:setvar {variable}[value]
  - Creates a sqlcmd scripting variable.
:xml on|off
  - Turns XML output formatting on or off.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_simple_commands_take_no_argument() {
        assert_eq!(parse("help"), Ok(Command::Help));
        assert_eq!(parse("quit"), Ok(Command::Quit));
        assert_eq!(parse("listvar"), Ok(Command::ListVar));
        assert_eq!(parse("reset"), Ok(Command::Reset));
    }

    #[test]
    fn command_names_are_case_insensitive() {
        assert_eq!(parse("HELP"), Ok(Command::Help));
        assert_eq!(parse("ListVar"), Ok(Command::ListVar));
    }

    #[test]
    fn the_three_exit_forms_are_distinguished() {
        assert_eq!(parse("exit"), Ok(Command::Exit(ExitForm::Immediate)));
        assert_eq!(parse("exit()"), Ok(Command::Exit(ExitForm::RunCache)));
        assert_eq!(
            parse("exit(SELECT 42)"),
            Ok(Command::Exit(ExitForm::Query("SELECT 42".into())))
        );
    }

    #[test]
    fn setvar_without_a_value_removes_the_variable() {
        assert_eq!(
            parse("setvar A"),
            Ok(Command::SetVar {
                name: "A".into(),
                value: None
            })
        );
        assert_eq!(
            parse("setvar A 1"),
            Ok(Command::SetVar {
                name: "A".into(),
                value: Some("1".into())
            })
        );
    }

    #[test]
    fn a_quoted_setvar_value_keeps_its_spaces() {
        assert_eq!(
            parse("setvar A \"one two\""),
            Ok(Command::SetVar {
                name: "A".into(),
                value: Some("one two".into())
            })
        );
    }

    #[test]
    fn on_error_takes_exit_or_ignore() {
        assert_eq!(parse("on error exit"), Ok(Command::OnError(OnError::Exit)));
        assert_eq!(
            parse("on error ignore"),
            Ok(Command::OnError(OnError::Ignore))
        );
        assert!(parse("on error maybe").is_err());
    }

    #[test]
    fn commands_that_need_an_argument_say_so() {
        assert_eq!(parse("r"), Err(ParseError::BadArguments("r".into())));
        assert_eq!(parse("out"), Err(ParseError::BadArguments("out".into())));
    }

    #[test]
    fn shell_escape_takes_the_rest_of_the_line() {
        assert_eq!(parse("!! dir /b"), Ok(Command::Shell("dir /b".into())));
        assert_eq!(parse("!!"), Ok(Command::Shell(String::new())));
    }

    #[test]
    fn an_unrecognised_word_is_not_a_command() {
        assert_eq!(parse("nonsense"), Err(ParseError::NotACommand));
    }
}

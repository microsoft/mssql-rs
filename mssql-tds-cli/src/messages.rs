// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! User-visible message catalog.
//!
//! Every string here mirrors an entry in the ODBC sqlcmd resource file
//! (`sqlcmd_lib.rc`) and is named after its `MSG_*` identifier. Text is
//! reproduced byte-for-byte, including the CRLF line endings the reference
//! emits on every platform; the differential tests compare against the shipped
//! binary, so edits here are behaviour changes.
//!
//! Routing every string through this module keeps a future translation catalog
//! a drop-in rather than a refactor.

/// The line terminator this platform's `sqlcmd` writes.
///
/// The ODBC message catalog spells its terminators `{EOL}`, and that is what the
/// Windows build emits; on Linux both references emit `\n`. A CR *inside a data
/// value* is passed through untouched by both, so the two cannot be told apart
/// at the output stream — the distinction has to be made here, where lines are
/// composed.
pub const EOL: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// `MSG_UNKNOWN_OPTION`
pub fn unknown_option(option: &str) -> String {
    format!("Sqlcmd: '{option}': Unknown Option. Enter '-?' for help.{EOL}")
}

/// `MSG_MISSING_ARG`
pub fn missing_arg(option: char) -> String {
    format!("Sqlcmd: '-{option}': Missing argument. Enter '-?' for help.{EOL}")
}

/// `MSG_UNEXPECTED_ARG`
pub fn unexpected_arg(arg: &str) -> String {
    format!("Sqlcmd: '{arg}': Unexpected argument. Enter '-?' for help.{EOL}")
}

/// `MSG_ARGUMENT_MISSING`
pub fn argument_missing() -> String {
    format!(
        "Sqlcmd: Error: '-' or '/' does not have an associated argument.{EOL}Enter '-?' for help.{EOL}"
    )
}

/// `MSG_OUTRANGE_ARG`
///
/// `subject` is the localized noun for the value being checked, such as
/// `Packet size` or `Timeout`.
pub fn outrange_arg(option: char, value: &str, subject: &str, min: i64, max: i64) -> String {
    format!(
        "Sqlcmd: '-{option} {value}': {subject} has to be a number between {min} and {max}.{EOL}"
    )
}

/// `MSG_COLWIDTH_OUTRANGE_ARG`
pub fn colwidth_outrange_arg(value: &str) -> String {
    format!("Sqlcmd: '-w {value}': value must be greater than 8 and less than 65536.{EOL}")
}

/// `MSG_SQLCMDMAXTYPEWIDTH_OUTRANGE_ARG`
pub fn maxtypewidth_outrange_arg(option: char, value: &str, min: i64, max: i64) -> String {
    format!(
        "Sqlcmd: '-{option} {value}': value must be greater than or equal to {min} and less than or equal to {max}.{EOL}"
    )
}

/// `MSG_REQUIRE_GREATEROREQUAL_NUMERIC_ARG`
pub fn require_greaterorequal_numeric_arg(value: &str, min: i64) -> String {
    format!(
        "Sqlcmd: '{value}': Unexpected argument. Argument has to be a number greater than or equal to {min}.{EOL}"
    )
}

/// `MSG_INVALID_HEADER_VALUE`
///
/// The reference emits this one without a trailing period.
pub fn invalid_header_value(value: &str) -> String {
    format!(
        "Sqlcmd: '-h {value}': header value must be either -1 or a value between -1 and 2147483647{EOL}"
    )
}

/// `MSG_OPT_SINGLE_USAGE`
pub fn opt_single_usage(option: char) -> String {
    format!(
        "Sqlcmd: The -{option} parameter can not be used in combination with other parameters.{EOL}"
    )
}

/// `MSG_OPTIONS_EXCLUSIVE`
pub fn options_exclusive(first: &str, second: &str) -> String {
    format!("Sqlcmd: The {first} and the {second} options are mutually exclusive.{EOL}")
}

/// `MSG_OPTIONS_EXCLUSIVE`, for a pair the two platforms word differently.
///
/// Windows names the option that was checked for and the group it excludes
/// (`The -E and the -U/-P options …`); Unix names the two bare letters, the
/// offending one first (`The U and the E options …`).
pub fn options_exclusive_pair(offender: char, primary: char, windows_pair: (&str, &str)) -> String {
    if cfg!(windows) {
        options_exclusive(windows_pair.0, windows_pair.1)
    } else {
        options_exclusive(&offender.to_string(), &primary.to_string())
    }
}

/// `MSG_MULTIPLE_SAME_OPT`
pub fn multiple_same_opt(option: &str) -> String {
    format!("Sqlcmd: Option '{option}' cannot be specified multiple times.{EOL}")
}

/// `MSG_INVALID_PARAMETERS`
pub fn invalid_parameters(option: char) -> String {
    format!("Sqlcmd: Command -{option}: Invalid Parameters passed.{EOL}")
}

/// `MSG_RETIRED_OPTIONS`
pub fn retired_option(option: char) -> String {
    format!("Sqlcmd: Warning: '-{option}' is an obsolete option and is ignored.{EOL}")
}

/// `MSG_RDONLY_VAR`
pub fn readonly_var(name: &str) -> String {
    format!("Sqlcmd: Error: The scripting variable: '{name}' is read-only.{EOL}")
}

/// `MSG_VAR_NOT_DEFINED`
pub fn var_not_defined(name: &str) -> String {
    format!("'{name}' scripting variable not defined.{EOL}")
}

/// `MSG_INVALID_VAR_NAME`
pub fn invalid_var_name(name: &str) -> String {
    format!("Sqlcmd: Error: Invalid variable identifier '{name}'.{EOL}")
}

/// `MSG_UNKNOWN_COMMAND`
pub fn unknown_command(command: &str) -> String {
    format!("Sqlcmd: Error: Unknown command '{command}'. Enter ':help' for help.{EOL}")
}

/// `MSG_SYNTAX_ERROR_CMD`
pub fn command_syntax_error(command: &str) -> String {
    format!("Sqlcmd: Error: Syntax error at command '{command}'. Enter ':help' for help.{EOL}")
}

/// `MSG_BASIC_ERRORINFO`
pub fn basic_errorinfo(source: &str, detail: &str) -> String {
    format!("Sqlcmd: Error: {source} : {detail}.{EOL}")
}

/// `MSG_FILE_OPEN_ERROR`
pub fn invalid_filename(path: &str) -> String {
    format!("'{path}': Invalid filename.{EOL}")
}

/// `MSG_FILE_OPEN_ERROR`, as reported for an `-i` file that cannot be opened.
pub fn invalid_input_filename(path: &str) -> String {
    format!("Sqlcmd: '{path}': Invalid filename.{EOL}")
}

/// `MSG_USER_TERMINATED`
pub fn user_terminated() -> String {
    format!(
        "Sqlcmd: Warning: The last operation was terminated because the user pressed CTRL+C.{EOL}"
    )
}

/// `MSG_GO_CMD_INVALID_PARAM`
pub fn go_invalid_param() -> String {
    format!("Sqlcmd: Error: Number of executions of the batch must be greater than zero.{EOL}")
}

/// `MSG_RECURSIVE_INCLUDE`
pub fn recursive_include(path: &str) -> String {
    format!(
        "Sqlcmd: Error: '{path}' is already being read; recursive includes are not allowed.{EOL}"
    )
}

/// An `--authentication-method` this build has no equivalent for.
pub fn unknown_auth_method(name: &str) -> String {
    format!("Sqlcmd: '{name}': Unsupported authentication method.{EOL}")
}

/// A `--compat` value naming neither tool.
pub fn unknown_compat(name: &str) -> String {
    format!("Sqlcmd: '{name}': Unknown compatibility mode. Use 'odbc' or 'go'.{EOL}")
}

/// `MSG_INVALID_VARIABLE_VALUE` — an environment variable naming something
/// unusable, such as a `SQLCMDINI` startup script that cannot be opened.
pub fn invalid_variable_value(name: &str, value: &str) -> String {
    format!("Sqlcmd: Error: The environment variable: '{name}' has invalid value: '{value}'.{EOL}")
}

/// A `-f` code page with no encoding behind it. Refusing beats falling back:
/// the caller asked for particular bytes and would otherwise get others.
pub fn invalid_code_page(code_page: u32) -> String {
    format!(
        "Sqlcmd: The code page <{code_page}> specified in option -f is invalid or not installed on this system.{EOL}"
    )
}

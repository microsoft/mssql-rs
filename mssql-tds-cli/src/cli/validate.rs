// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Turning lexed options into a validated configuration.
//!
//! Range limits, defaults and conflict rules are those of ODBC sqlcmd, checked
//! against the shipped binary rather than taken from documentation — several
//! published limits are wrong (`-y` tops out at 8000, `-V` starts at 1, and
//! `-q` and `-Q` are not in fact mutually exclusive).

use crate::compat::Compat;
use crate::messages;

use super::args::{CliError, Lexed, RawOption};
use super::spec;

const DEFAULT_PACKET_SIZE: i64 = 4096;
const DEFAULT_LOGIN_TIMEOUT: i64 = 8;
/// go-sqlcmd waits considerably longer before giving up on a login.
const GO_LOGIN_TIMEOUT: i64 = 30;
/// `SQLCMDCOLWIDTH` defaults to 0, meaning lines are never wrapped.
const DEFAULT_COLUMN_WIDTH: i64 = 0;
const DEFAULT_VAR_TYPE_WIDTH: i64 = 256;
const MAX_TYPE_WIDTH: i64 = 8000;

/// What `-N` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encrypt {
    /// `-N` with no suffix.
    On,
    /// `-Ns`
    Strict,
    /// `-Nm`
    Mandatory,
    /// `-No`
    Optional,
}

/// What `-k` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlChars {
    /// `-k` — drop control characters.
    Remove,
    /// `-k1` — one space per character.
    SpacePerChar,
    /// `-k2` — one space per run.
    SpacePerRun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub server: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub new_password: Option<String>,
    pub exit_after_password_change: bool,
    pub database: Option<String>,
    /// `-D`: an ODBC data source name to draw connection settings from.
    pub dsn: Option<String>,
    pub workstation: Option<String>,
    pub trusted_connection: bool,
    pub dedicated_admin_connection: bool,
    pub use_entra_id: bool,
    pub application_intent: Option<String>,
    pub multi_subnet_failover: bool,

    pub encrypt: Option<Encrypt>,
    pub trust_server_certificate: bool,
    pub host_name_in_certificate: Option<String>,
    pub server_certificate: Option<String>,
    pub column_encryption: bool,

    pub packet_size: i64,
    pub login_timeout: i64,
    pub query_timeout: i64,

    pub input_files: Vec<String>,
    pub output_file: Option<String>,
    pub initial_query: Option<String>,
    pub query_and_exit: Option<String>,
    pub batch_terminator: String,
    pub variables: Vec<String>,

    pub headers: i64,
    pub column_separator: String,
    pub column_width: i64,
    pub var_type_width: i64,
    pub fixed_type_width: i64,
    pub trim_columns: bool,
    pub control_chars: Option<ControlChars>,
    pub unicode_output: bool,
    /// `-f` input code page, if one was given.
    pub input_code_page: Option<u32>,
    /// `-f` output code page, if one was given.
    pub output_code_page: Option<u32>,
    /// `-R`. Accepted and ignored: it asks for the client's locale to drive
    /// number and date formatting, which differs from the invariant form only
    /// on non-English locales. go-sqlcmd ignores it for the same reason.
    pub use_regional_settings: bool,

    pub error_level: i64,
    pub severity_level: i64,
    pub exit_on_error: bool,
    pub raw_error_messages: bool,
    pub errors_to_stderr: i64,

    pub echo_input: bool,
    pub quoted_identifiers: bool,
    pub disable_variable_substitution: bool,
    pub disable_commands: bool,
    pub disable_commands_and_exit: bool,
    pub print_statistics: bool,
    pub statistics_colon_format: bool,

    pub list_servers: bool,
    pub list_servers_clean: bool,

    // Added by go-sqlcmd.
    /// `--vertical` / `--ascii` / `--format`.
    pub format: Option<crate::fmt::layout::Format>,
    /// `--authentication-method`: names an Entra method outright instead of
    /// inferring one from `-G` and the credentials beside it.
    pub authentication_method: Option<String>,
    /// `--server-name`: the name to present at login, when it differs from the
    /// address dialled. Needed when reaching the server through a tunnel or
    /// port-forward.
    pub login_server_name: Option<String>,
    /// `--driver-logging-level`.
    pub driver_logging_level: i64,
    /// `--trace-file`.
    pub trace_file: Option<String>,
    /// `--compat`: whose behaviour to follow where ODBC and go-sqlcmd differ.
    pub compat: Compat,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            server: None,
            user: None,
            password: None,
            new_password: None,
            exit_after_password_change: false,
            database: None,
            dsn: None,
            workstation: None,
            trusted_connection: false,
            dedicated_admin_connection: false,
            use_entra_id: false,
            application_intent: None,
            multi_subnet_failover: false,
            encrypt: None,
            trust_server_certificate: false,
            host_name_in_certificate: None,
            server_certificate: None,
            column_encryption: false,
            packet_size: DEFAULT_PACKET_SIZE,
            login_timeout: DEFAULT_LOGIN_TIMEOUT,
            query_timeout: 0,
            input_files: Vec::new(),
            output_file: None,
            initial_query: None,
            query_and_exit: None,
            batch_terminator: "GO".to_string(),
            variables: Vec::new(),
            headers: 0,
            column_separator: " ".to_string(),
            column_width: DEFAULT_COLUMN_WIDTH,
            var_type_width: DEFAULT_VAR_TYPE_WIDTH,
            fixed_type_width: 0,
            trim_columns: false,
            control_chars: None,
            unicode_output: false,
            input_code_page: None,
            output_code_page: None,
            use_regional_settings: false,
            error_level: 0,
            severity_level: 0,
            exit_on_error: false,
            raw_error_messages: false,
            errors_to_stderr: -1,
            echo_input: false,
            quoted_identifiers: false,
            disable_variable_substitution: false,
            disable_commands: false,
            disable_commands_and_exit: false,
            print_statistics: false,
            statistics_colon_format: false,
            list_servers: false,
            list_servers_clean: false,
            format: None,
            authentication_method: None,
            login_server_name: None,
            driver_logging_level: 0,
            trace_file: None,
            compat: Compat::default(),
        }
    }
}

impl Options {
    /// `-t 0` means no limit, which the driver spells as `None`.
    pub fn query_timeout_option(&self) -> Option<u32> {
        match u32::try_from(self.query_timeout).unwrap_or(0) {
            0 => None,
            seconds => Some(seconds),
        }
    }
}

/// `-f` accepts a bare code page, or `i:` and `o:` parts naming the input and
/// output sides separately: `-f 65001`, `-f i:1252`, `-f i:1252,o:65001`.
fn code_pages(text: &str) -> Option<(Option<u32>, Option<u32>)> {
    let text = text.trim();
    if let Ok(both) = text.parse::<u32>() {
        return Some((Some(both), Some(both)));
    }

    let (mut input, mut output) = (None, None);
    for part in text.split(',') {
        let part = part.trim();
        let (side, number) = part.split_once(':')?;
        let number = number.trim().parse::<u32>().ok()?;
        match side.trim() {
            "i" | "I" => input = Some(number),
            "o" | "O" => output = Some(number),
            _ => return None,
        }
    }
    (input.is_some() || output.is_some()).then_some((input, output))
}

fn value(opt: &RawOption) -> &str {
    opt.value.as_deref().unwrap_or_default()
}
fn integer(opt: &RawOption) -> Option<i64> {
    value(opt).trim().parse::<i64>().ok()
}

fn ranged(opt: &RawOption, subject: &str, min: i64, max: i64) -> Result<i64, CliError> {
    match integer(opt) {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(CliError::Stderr(messages::outrange_arg(
            opt.short,
            value(opt),
            subject,
            min,
            max,
        ))),
    }
}

fn type_width(opt: &RawOption) -> Result<i64, CliError> {
    match integer(opt) {
        Some(n) if (0..=MAX_TYPE_WIDTH).contains(&n) => Ok(n),
        _ => Err(CliError::Stderr(messages::maxtypewidth_outrange_arg(
            opt.short,
            value(opt),
            0,
            MAX_TYPE_WIDTH,
        ))),
    }
}

fn check_conflicts(lexed: &Lexed) -> Result<(), CliError> {
    // go-sqlcmd refuses two output layouts at once.
    if lexed.contains(spec::VERTICAL) && lexed.contains(spec::ASCII) {
        return Err(CliError::Stderr(messages::options_exclusive(
            "--vertical",
            "--ascii",
        )));
    }
    // `-G` infers a method; `--authentication-method` states one.
    if lexed.contains('G') && lexed.contains(spec::AUTH_METHOD) {
        return Err(CliError::Stderr(messages::options_exclusive(
            "-G",
            "--authentication-method",
        )));
    }
    if lexed.contains('L') && lexed.options.len() > 1 {
        return Err(CliError::Stderr(messages::opt_single_usage('L')));
    }
    if lexed.contains('E')
        && let Some(offender) = ['U', 'P'].into_iter().find(|o| lexed.contains(*o))
    {
        return Err(CliError::Stderr(messages::options_exclusive_pair(
            offender,
            'E',
            ("-E", "-U/-P"),
        )));
    }
    if lexed.contains('E')
        && let Some(offender) = ['z', 'Z'].into_iter().find(|o| lexed.contains(*o))
    {
        return Err(CliError::Stderr(messages::options_exclusive_pair(
            offender,
            'E',
            ("-E", "-z/-Z"),
        )));
    }
    if lexed.contains('W')
        && let Some(offender) = ['y', 'Y'].into_iter().find(|o| lexed.contains(*o))
    {
        return Err(CliError::Stderr(messages::options_exclusive_pair(
            offender,
            'W',
            ("-W", "-y/-Y"),
        )));
    }
    // Only the Windows reference treats these as exclusive; on Unix it opens
    // the input file and lets that fail on its own terms.
    if cfg!(windows) && lexed.contains('i') && (lexed.contains('q') || lexed.contains('Q')) {
        return Err(CliError::Stderr(messages::options_exclusive("i", "-Q/-q")));
    }
    if lexed.count('o') > 1 {
        return Err(CliError::Stdout(messages::multiple_same_opt("-o")));
    }
    Ok(())
}

/// `--compat` beats `SQLCMDCOMPAT`, which beats the ODBC default. Text that
/// names neither tool is handed back so the caller can refuse it: a typo in
/// the environment must not quietly leave the wrong mode running.
fn resolve_compat(flag: Option<&str>, from_env: Option<&str>) -> Result<Compat, String> {
    let chosen = match flag {
        Some(text) => Some(text),
        None => from_env.filter(|text| !text.trim().is_empty()),
    };
    match chosen {
        Some(text) => Compat::parse(text).ok_or_else(|| text.to_string()),
        None => Ok(Compat::default()),
    }
}

/// Apply lexed options over the defaults.
pub fn resolve(lexed: &Lexed) -> Result<Options, CliError> {
    check_conflicts(lexed)?;

    let mut o = Options::default();
    let mut headers_set = false;
    // `--compat` may appear after the options whose defaults it changes, so the
    // mode is settled first and the affected defaults applied before the pass.
    let flag = lexed
        .options
        .iter()
        .find(|opt| opt.short == spec::COMPAT)
        .map(value);
    let from_env = std::env::var("SQLCMDCOMPAT").ok();
    o.compat = resolve_compat(flag, from_env.as_deref())
        .map_err(|text| CliError::Stderr(messages::unknown_compat(&text)))?;
    if o.compat.is_go() {
        o.login_timeout = GO_LOGIN_TIMEOUT;
    }

    for opt in &lexed.options {
        match opt.short {
            '?' => {}
            'S' => o.server = Some(value(opt).to_string()),
            'U' => o.user = Some(value(opt).to_string()),
            'P' => o.password = Some(value(opt).to_string()),
            'z' | 'Z' => {
                o.new_password = Some(value(opt).to_string());
                o.exit_after_password_change = opt.short == 'Z';
            }
            'd' => o.database = Some(value(opt).to_string()),
            'D' => o.dsn = Some(value(opt).to_string()),
            'H' => o.workstation = Some(value(opt).to_string()),
            'E' => o.trusted_connection = true,
            'A' => o.dedicated_admin_connection = true,
            'G' => o.use_entra_id = true,
            'K' => o.application_intent = Some(value(opt).to_string()),
            'M' => o.multi_subnet_failover = true,

            'N' => {
                o.encrypt = Some(match opt.suffix {
                    None => Encrypt::On,
                    Some('s') => Encrypt::Strict,
                    Some('m') => Encrypt::Mandatory,
                    Some(_) => Encrypt::Optional,
                })
            }
            'C' => o.trust_server_certificate = true,
            'F' => o.host_name_in_certificate = Some(value(opt).to_string()),
            'J' => o.server_certificate = Some(value(opt).to_string()),
            'g' => o.column_encryption = true,

            'a' => o.packet_size = ranged(opt, "Packet size", 512, 32767)?,
            'l' => o.login_timeout = ranged(opt, "Timeout", 0, 65534)?,
            't' => o.query_timeout = ranged(opt, "Timeout", 0, 65534)?,

            'i' => o
                .input_files
                .extend(value(opt).split(',').map(str::to_string)),
            'o' => o.output_file = Some(value(opt).to_string()),
            'q' => o.initial_query = Some(value(opt).to_string()),
            'Q' => o.query_and_exit = Some(value(opt).to_string()),
            'c' => o.batch_terminator = value(opt).to_string(),
            'v' => o.variables.push(value(opt).to_string()),

            'h' => {
                let n = integer(opt)
                    .filter(|n| *n >= -1)
                    .ok_or_else(|| CliError::Stderr(messages::invalid_header_value(value(opt))))?;
                o.headers = n;
                headers_set = true;
            }
            's' => o.column_separator = value(opt).to_string(),
            'w' => {
                o.column_width = integer(opt)
                    .filter(|n| (9..=65535).contains(n))
                    .ok_or_else(|| CliError::Stderr(messages::colwidth_outrange_arg(value(opt))))?;
            }
            'y' => o.var_type_width = type_width(opt)?,
            'Y' => o.fixed_type_width = type_width(opt)?,
            'W' => o.trim_columns = true,
            'k' => {
                o.control_chars = Some(match opt.suffix {
                    None => ControlChars::Remove,
                    Some('1') => ControlChars::SpacePerChar,
                    Some(_) => ControlChars::SpacePerRun,
                })
            }
            'u' => o.unicode_output = true,
            'f' => {
                let (input, output) = code_pages(value(opt))
                    .ok_or_else(|| CliError::Stderr(messages::invalid_parameters('f')))?;
                // A code page with no encoding behind it must be refused:
                // falling back to UTF-8 would silently write bytes the caller
                // did not ask for, and they would have no way to tell.
                for code_page in [input, output].into_iter().flatten() {
                    if crate::io::encoding_for_code_page(code_page).is_none() {
                        return Err(CliError::Stderr(messages::invalid_code_page(code_page)));
                    }
                }
                o.input_code_page = input;
                o.output_code_page = output;
            }
            'R' => o.use_regional_settings = true,

            'm' => {
                o.error_level = integer(opt).filter(|n| *n >= -1).ok_or_else(|| {
                    CliError::Stderr(messages::require_greaterorequal_numeric_arg(value(opt), -1))
                })?;
            }
            'V' => o.severity_level = ranged(opt, "Severity level", 1, 25)?,
            'b' => o.exit_on_error = true,
            'j' => o.raw_error_messages = true,
            'r' => o.errors_to_stderr = if opt.suffix == Some('1') { 1 } else { 0 },

            'e' => o.echo_input = true,
            'I' => o.quoted_identifiers = true,
            'x' => o.disable_variable_substitution = true,
            'X' => {
                o.disable_commands = true;
                o.disable_commands_and_exit = opt.suffix == Some('1');
            }
            'p' => {
                o.print_statistics = true;
                o.statistics_colon_format = opt.suffix == Some('1');
            }

            'L' => {
                o.list_servers = true;
                o.list_servers_clean = opt.suffix == Some('c');
            }

            // Added by go-sqlcmd.
            spec::VERTICAL => o.format = Some(crate::fmt::layout::Format::Vertical),
            spec::ASCII => o.format = Some(crate::fmt::layout::Format::Ascii),
            spec::FORMAT => o.format = Some(crate::fmt::layout::Format::parse(value(opt))),
            // `--version` is handled before options are resolved.
            spec::VERSION => {}
            spec::AUTH_METHOD => {
                let name = value(opt);
                if crate::exec::connect::named_method(name).is_none() {
                    return Err(CliError::Stderr(messages::unknown_auth_method(name)));
                }
                o.authentication_method = Some(name.to_string());
            }
            // Dialling one address while presenting another at login. The
            // driver carries the override through LOGIN7, so a connection via
            // a tunnel or port-forward can still name the real server.
            spec::SERVER_NAME => o.login_server_name = Some(value(opt).to_string()),
            spec::DRIVER_LOGGING => o.driver_logging_level = integer(opt).unwrap_or(0),
            spec::TRACE_FILE => o.trace_file = Some(value(opt).to_string()),
            spec::COMPAT => {
                // Settled before the loop, since it changes other defaults.
            }

            // `-T` is accepted by the grammar but not acted on.
            'T' => {}

            other => unreachable!("lexer admitted an unhandled option -{other}"),
        }
    }

    // Unlimited variable-type width forces headers off, but only if the user
    // did not ask for a specific header interval.
    if o.var_type_width == 0 {
        if headers_set {
            return Err(CliError::Stderr(messages::options_exclusive("-h", "-y 0")));
        }
        o.headers = -1;
    }

    Ok(o)
}

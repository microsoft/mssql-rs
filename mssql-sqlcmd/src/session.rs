// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The running tool: input, command dispatch, batch execution and exit codes.

use std::path::Path;

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::core::CancelHandle;

use crate::batch::{self, Batch, LineKind};
use crate::cli::validate::Options;
use crate::commands::{self, Command, ExitForm, OnError, ParseError};
use crate::exec::connect;
use crate::exec::runner::{self, Output, RunStyle};
use crate::exitcode;
use crate::fmt::color::{self, Colorizer, TextType};
use crate::fmt::layout::Format;
use crate::fmt::report;
use crate::fmt::table::TableStyle;
use crate::io::{Destination, OutputEncoding, Sink, encoding_for_code_page};
use crate::messages;
use crate::messages::EOL;
use crate::servers;
use crate::tracing;
use crate::vars::{SetError, Variables};

/// Why the read loop stopped.
enum Stop {
    /// Input ran out.
    EndOfInput,
    /// `:exit`, `:quit`, or an error under `:on error exit` / `-b`.
    Requested(i32),
}

pub struct Session {
    options: Options,
    vars: Variables,
    batch: Batch,
    client: Option<TdsClient>,
    cancel: CancelHandle,
    results: Sink,
    errors: Sink,
    on_error: OnError,
    /// Highest severity seen, for the `-V` exit-code threshold.
    highest_severity: i32,
    exit_code: i32,
    /// `:r` ancestry, so a file cannot include itself.
    including: Vec<String>,
    /// `:xml on` suppresses the tabular layout and prints raw column text.
    xml_mode: bool,
    /// The layout results are drawn in.
    format: Format,
    /// `SQLCMDCOLORSCHEME`. Inactive unless a known scheme is named and the
    /// results are going to a terminal.
    colors: Colorizer,
}

impl Session {
    pub fn new(mut options: Options) -> std::io::Result<Self> {
        let workstation = hostname();

        // `-U` without `-P` asks at the console rather than failing.
        if options.user.is_some() && options.password.is_none() && !options.trusted_connection {
            options.password = prompt_for_password();
        }

        let mut vars = Variables::with_defaults(&workstation, options.compat);
        // `-X` blocks environment seeding under go-sqlcmd but not under ODBC,
        // which still reads `SQLCMDCOLWIDTH`, `SQLCMDINI` and the rest —
        // measured with `:listvar` against both references. That is also why
        // ODBC still runs the startup script under `-X`: the variable naming it
        // was seeded in the first place.
        let seeding_suppressed = options.compat.is_go()
            && (options.disable_commands || options.disable_commands_and_exit);
        if !seeding_suppressed {
            vars.seed_from_environment();
        }
        seed_from_options(&mut vars, &options, &workstation);

        let encoding = output_encoding(&options);

        // An explicit flag beats `SQLCMDFORMAT`, which beats the ODBC layout.
        let format = options.format.unwrap_or_else(|| {
            vars.get("SQLCMDFORMAT")
                .map(Format::parse)
                .unwrap_or_default()
        });

        let results = match &options.output_file {
            Some(path) => Sink::file(Path::new(path), encoding, false)?,
            None => Sink::stdout(encoding),
        };

        // `-o` sends results to a file, which is never a terminal, so the
        // scheme is resolved against where they are actually going.
        let colors = Colorizer::new(
            vars.get("SQLCMDCOLORSCHEME").unwrap_or_default(),
            options.output_file.is_none() && color::stdout_is_terminal(),
        );

        Ok(Self {
            on_error: if options.exit_on_error {
                OnError::Exit
            } else {
                OnError::Ignore
            },
            options,
            vars,
            batch: Batch::default(),
            client: None,
            cancel: CancelHandle::new(),
            results,
            // sqlcmd's own diagnostics go to stderr; server messages go to the
            // results stream unless `-r` moves them here.
            errors: Sink::stderr(),
            highest_severity: 0,
            exit_code: exitcode::SUCCESS,
            including: Vec::new(),
            xml_mode: false,
            format,
            colors,
        })
    }

    /// Connects, runs whatever the options ask for, and returns the exit code.
    pub async fn run(mut self) -> i32 {
        // `-L` never connects; it asks the network who is out there.
        if self.options.list_servers {
            let listing = servers::list(self.options.list_servers_clean).await;
            self.results.write(&listing);
            self.results.flush();
            return exitcode::SUCCESS;
        }

        // An unreadable `-i` file is reported before connecting, so a bad path
        // is not hidden behind a connection error.
        for path in &self.options.input_files {
            if std::fs::File::open(path).is_err() {
                self.errors.write(&messages::invalid_input_filename(path));
                self.errors.flush();
                return exitcode::FAILURE;
            }
        }

        if let Err(text) = Box::pin(self.open_connection()).await {
            self.errors.write(&text);
            self.errors.flush();
            return exitcode::FAILURE;
        }

        // `-Z` changes the password and stops; `-z` changes it and carries on.
        if self.options.exit_after_password_change {
            self.results.flush();
            if let Some(client) = &mut self.client {
                let _ = client.close_connection().await;
            }
            return exitcode::SUCCESS;
        }

        // The startup script runs before any user input. Under go-sqlcmd `-X`
        // leaves `SQLCMDINI` unseeded, so there is nothing to run; under ODBC
        // the variable survives and the script still runs.
        if let Some(path) = self.vars.get("SQLCMDINI").map(str::to_string)
            && !path.is_empty()
        {
            match self.read_input(&path) {
                Some(text) => {
                    Box::pin(self.feed(&text)).await;
                }
                // Both references name the variable and its value rather than
                // reporting a bare missing file.
                None => self
                    .errors
                    .write(&messages::invalid_variable_value("SQLCMDINI", &path)),
            }
        }

        if let Some(query) = self.options.initial_query.clone() {
            self.run_text(&query, 1).await;
        }

        let mut stop = Stop::EndOfInput;

        if let Some(query) = self.options.query_and_exit.clone() {
            self.run_text(&query, 1).await;
        } else if !self.options.input_files.is_empty() {
            for path in self.options.input_files.clone() {
                match self.read_input(&path) {
                    Some(text) => {
                        if let Stop::Requested(code) = Box::pin(self.feed(&text)).await {
                            stop = Stop::Requested(code);
                            break;
                        }
                    }
                    None => {
                        self.errors.write(&messages::invalid_input_filename(&path));
                        self.exit_code = exitcode::FAILURE;
                    }
                }
            }
        } else {
            stop = Box::pin(self.interactive()).await;
        }

        // Anything still in the cache at end of input is run, as the reference does.
        if let Stop::EndOfInput = stop
            && !self.batch.is_empty()
        {
            self.execute_cache(1).await;
        }

        self.results.flush();
        self.errors.flush();

        if let Some(client) = &mut self.client {
            let _ = client.close_connection().await;
        }

        match stop {
            Stop::Requested(code) => code,
            Stop::EndOfInput => self.final_exit_code(),
        }
    }

    fn final_exit_code(&self) -> i32 {
        if self.exit_code != exitcode::SUCCESS {
            return self.exit_code;
        }
        // `-V n` turns a message at or above severity n into an exit code that
        // *is* that severity, rather than a plain failure.
        if self.options.severity_level > 0
            && self.highest_severity >= self.options.severity_level as i32
        {
            return self.highest_severity;
        }
        if self.options.exit_on_error && self.highest_severity > self.options.error_level as i32 {
            return exitcode::FAILURE;
        }
        exitcode::SUCCESS
    }

    async fn open_connection(&mut self) -> Result<(), String> {
        let workstation = hostname();
        let (context, source) = connect::build_context(&self.options, &workstation);
        match connect::connect(context, &source, Some(&self.cancel)).await {
            Ok(mut client) => {
                // The reference leaves QUOTED_IDENTIFIER off unless `-I` asks
                // for it, which is the opposite of most other clients.
                let setting = if self.options.quoted_identifiers {
                    "ON"
                } else {
                    "OFF"
                };
                let _ = client
                    .execute(format!("SET QUOTED_IDENTIFIER {setting}"), None, None)
                    .await;
                let _ = client.close_query().await;
                let _ = client.take_info_messages();
                let _ = client.take_done_row_counts();

                // The reference reports what `-S` was given, port and all,
                // rather than the host it parsed out of it.
                if let Some(server) = &self.options.server {
                    self.vars.set_internal("SQLCMDSERVER", server);
                }
                // go-sqlcmd reports only what the caller asked for, leaving
                // `SQLCMDDBNAME` empty when `-d` was not given; ODBC fills in
                // whichever database the login landed in.
                if !self.options.compat.is_go() {
                    self.vars.set_internal("SQLCMDDBNAME", client.database());
                }
                self.client = Some(client);
                Ok(())
            }
            Err(error) => Err(connection_error_text(
                &error,
                self.options.raw_error_messages,
            )),
        }
    }

    /// Reads from stdin, prompting when a terminal is attached.
    ///
    /// On a terminal this goes through `rustyline`, which brings line editing
    /// and history. Piped input takes the plain path instead: an editor on a
    /// non-terminal would strip the very bytes the batch parser needs, and the
    /// differential harnesses all pipe.
    async fn interactive(&mut self) -> Stop {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            self.interactive_editor().await
        } else {
            self.interactive_piped().await
        }
    }

    /// The line-edited path: history, recall and editing at the `1>` prompt.
    async fn interactive_editor(&mut self) -> Stop {
        let mut editor = match rustyline::DefaultEditor::new() {
            Ok(editor) => editor,
            // No terminal to drive after all; fall back rather than fail.
            Err(_) => return Box::pin(self.interactive_piped()).await,
        };

        loop {
            // The results stream may be redirected, so the prompt goes through
            // rustyline rather than `prompt()`, which writes to that stream.
            let prompt = format!("{}> ", self.batch.line_count() + 1);
            match editor.readline(&prompt) {
                Ok(line) => {
                    // Only whole statements are worth recalling; a bare
                    // continuation line on its own is noise in the history.
                    if !line.trim().is_empty() {
                        let _ = editor.add_history_entry(line.as_str());
                    }
                    // rustyline hands back the line without its terminator, and
                    // the batch parser needs one to close the line.
                    let eol = if cfg!(windows) { "\r\n" } else { "\n" };
                    if let Some(stop) = Box::pin(self.line(&line, eol)).await {
                        return stop;
                    }
                }
                // Ctrl+C abandons the half-typed batch, as the reference does,
                // and leaves the session running.
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    self.batch.reset();
                }
                // Ctrl+D, or the terminal going away.
                Err(_) => return Stop::EndOfInput,
            }
        }
    }

    /// The plain path, for piped or redirected input.
    async fn interactive_piped(&mut self) -> Stop {
        use std::io::BufRead;

        let stdin = std::io::stdin();
        let mut line = String::new();

        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => return Stop::EndOfInput,
                Ok(_) => {}
                Err(_) => return Stop::EndOfInput,
            }
            let text = line.trim_end_matches(['\r', '\n']);
            let eol = &line[text.len()..];
            if let Some(stop) = Box::pin(self.line(text, eol)).await {
                return stop;
            }
        }
    }

    /// The terminator to record for an input line.
    ///
    /// ODBC keeps whatever the input had, so an LF script yields LF inside a
    /// multi-line literal. go-sqlcmd rewrites every terminator to the
    /// platform's own as it assembles the batch.
    fn batch_eol<'a>(&self, original: &'a str) -> &'a str {
        if self.options.compat.is_go() && !original.is_empty() {
            if cfg!(windows) { "\r\n" } else { "\n" }
        } else {
            original
        }
    }

    /// Feeds a whole script through the same path as interactive input.
    async fn feed(&mut self, text: &str) -> Stop {
        for (raw, eol) in batch::split_lines(text) {
            if let Some(stop) = Box::pin(self.line(raw, eol)).await {
                return stop;
            }
        }
        Stop::EndOfInput
    }

    /// Handles one input line. `Some(..)` means stop.
    async fn line(&mut self, text: &str, eol: &str) -> Option<Stop> {
        let eol = self.batch_eol(eol);
        let terminator = self.options.batch_terminator.clone();
        match self.batch.push_line(text, eol, &terminator) {
            LineKind::Buffered => {
                // `-e` echoes the statement text only; terminators and colon
                // commands are not part of what gets sent.
                if self.options.echo_input {
                    self.results.write(&format!("{text}{EOL}"));
                }
                None
            }
            LineKind::Terminator { count } => {
                if count == 0 {
                    self.errors.write(&messages::go_invalid_param());
                    return None;
                }
                // ODBC separates the echoed statement from its results with a
                // blank line; go-sqlcmd runs them together.
                if self.options.echo_input && !self.batch.is_empty() && !self.options.compat.is_go()
                {
                    self.results.write(EOL);
                }
                self.execute_cache(count).await;
                self.stop_if_failed()
            }
            LineKind::Command(command) => Box::pin(self.command(&command, text, eol)).await,
        }
    }

    fn stop_if_failed(&mut self) -> Option<Stop> {
        let failed = self.highest_severity > self.options.error_level as i32;
        if failed && matches!(self.on_error, OnError::Exit) {
            return Some(Stop::Requested(exitcode::FAILURE));
        }
        None
    }

    async fn command(&mut self, text: &str, raw: &str, eol: &str) -> Option<Stop> {
        if self.options.disable_commands_and_exit && is_disabled(text) {
            self.errors.write(&messages::unknown_command(text));
            return Some(Stop::Requested(exitcode::FAILURE));
        }

        let command = match commands::parse(text) {
            Ok(command) => command,
            // Not a command after all — the reference sends the line onward as
            // ordinary text and lets the server object to it.
            Err(ParseError::NotACommand) => {
                self.batch.push_text(raw, eol);
                if self.options.echo_input {
                    self.results.write(&format!("{raw}{EOL}"));
                }
                return None;
            }
            Err(ParseError::BadArguments(word)) => {
                self.errors.write(&messages::command_syntax_error(&word));
                return None;
            }
        };

        match command {
            Command::Help => {
                self.results.write(&crlf(commands::HELP));
                None
            }
            Command::Quit => Some(Stop::Requested(self.final_exit_code())),
            Command::Exit(form) => self.exit(form).await,
            Command::List => {
                self.results.write(&crlf(self.batch.text()));
                None
            }
            Command::ListColor => {
                // Each scheme against a sample statement, so the effect of one
                // can be seen before it is chosen.
                const SAMPLE: &str =
                    "select 'literal' as literal, 100 as number from [sys].[tables]";
                let listing: String = Colorizer::names()
                    .iter()
                    .map(|name| format!("{name}: {SAMPLE}{EOL}"))
                    .collect();
                self.results.write(&listing);
                None
            }
            Command::ListVar => {
                let listing: Vec<String> = self
                    .vars
                    .listing()
                    .iter()
                    .map(|(name, value)| format!("{name} = \"{value}\"{EOL}"))
                    .collect();
                self.results.write(&listing.concat());
                None
            }
            Command::Reset => {
                self.batch.reset();
                None
            }
            Command::SetVar { name, value } => {
                self.set_var(&name, value.as_deref());
                None
            }
            Command::Read(path) => {
                self.include(&path).await;
                None
            }
            Command::Out(target) => {
                self.redirect_results(&target);
                None
            }
            Command::Error(target) => {
                self.redirect_errors(&target);
                None
            }
            Command::OnError(action) => {
                self.on_error = action;
                None
            }
            Command::Connect(argument) => {
                Box::pin(self.reconnect(&argument)).await;
                None
            }
            Command::Shell(line) => {
                self.shell(&line);
                None
            }
            Command::Editor => {
                self.edit();
                None
            }
            Command::Xml(on) => {
                self.xml_mode = on;
                None
            }
            // Timing traces and server enumeration from the prompt are
            // accepted so existing scripts keep working, but do nothing.
            Command::PerfTrace(_) | Command::ServerList => None,
        }
    }

    fn set_var(&mut self, name: &str, value: Option<&str>) {
        let outcome = match value {
            Some(value) => self.vars.set(name, value),
            None => self.vars.remove(name),
        };
        match outcome {
            Ok(()) => {}
            Err(SetError::ReadOnly) => self.errors.write(&messages::readonly_var(name)),
            Err(SetError::InvalidName) => self.errors.write(&messages::invalid_var_name(name)),
        }
    }

    /// Reads a script, decoding it with the `-f` input code page when one was
    /// given and as UTF-8 otherwise.
    fn read_input(&self, path: &str) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        match self
            .options
            .input_code_page
            .and_then(encoding_for_code_page)
        {
            Some(encoding) => Some(encoding.decode(&bytes).0.into_owned()),
            None => Some(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }

    /// `:r` — splice a file into the statement cache.
    ///
    /// A file that includes itself, directly or through a chain, would recurse
    /// forever, so the ancestry is tracked and a repeat is refused.
    async fn include(&mut self, path: &str) {
        let key = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string());
        if self.including.contains(&key) {
            self.errors.write(&messages::recursive_include(path));
            return;
        }

        match self.read_input(path) {
            Some(text) => {
                self.including.push(key);
                let terminator = self.options.batch_terminator.clone();
                for (raw, eol) in batch::split_lines(&text) {
                    let eol = self.batch_eol(eol);
                    match self.batch.push_line(raw, eol, &terminator) {
                        LineKind::Terminator { count } => self.execute_cache(count).await,
                        LineKind::Command(command) => {
                            Box::pin(self.command(&command, raw, eol)).await;
                        }
                        LineKind::Buffered => {}
                    }
                }
                self.including.pop();
            }
            None => self.errors.write(&messages::invalid_filename(path)),
        }
    }

    /// `:ed` — hand the statement cache to the editor and take back whatever
    /// it saved.
    fn edit(&mut self) {
        if self.options.disable_commands || self.options.disable_commands_and_exit {
            return;
        }

        let editor = self
            .vars
            .get("SQLCMDEDITOR")
            .unwrap_or("edit.com")
            .to_string();
        let path = std::env::temp_dir().join(format!("sqlcmd-{}.sql", std::process::id()));

        if std::fs::write(&path, self.batch.text()).is_err() {
            self.errors
                .write(&messages::invalid_filename(&path.to_string_lossy()));
            return;
        }

        let status = std::process::Command::new(&editor).arg(&path).status();
        if status.is_err() {
            self.errors
                .write(&messages::basic_errorinfo("Sqlcmd", "Editor not found"));
            let _ = std::fs::remove_file(&path);
            return;
        }

        if let Ok(text) = std::fs::read_to_string(&path) {
            self.batch.reset();
            let terminator = self.options.batch_terminator.clone();
            for (line, eol) in batch::split_lines(&text) {
                let eol = self.batch_eol(eol);
                self.batch.push_line(line, eol, &terminator);
            }
            // The reference shows the edited text back to you.
            self.results.write(&crlf(self.batch.text()));
        }
        let _ = std::fs::remove_file(&path);
    }

    fn redirect_results(&mut self, target: &str) {
        let encoding = output_encoding(&self.options);
        match Destination::parse(target) {
            Destination::Stdout => self.results = Sink::stdout(encoding),
            Destination::Stderr => self.results = Sink::stderr(),
            Destination::File(path) => match Sink::file(Path::new(&path), encoding, false) {
                Ok(sink) => self.results = sink,
                Err(_) => self.errors.write(&messages::invalid_filename(&path)),
            },
        }
    }

    fn redirect_errors(&mut self, target: &str) {
        match Destination::parse(target) {
            Destination::Stdout => self.errors = Sink::stdout(OutputEncoding::Utf8),
            Destination::Stderr => self.errors = Sink::stderr(),
            Destination::File(path) => {
                match Sink::file(Path::new(&path), OutputEncoding::Utf8, false) {
                    Ok(sink) => self.errors = sink,
                    Err(_) => self.errors.write(&messages::invalid_filename(&path)),
                }
            }
        }
    }

    async fn reconnect(&mut self, argument: &str) {
        let mut options = self.options.clone();
        let mut words = argument.split_whitespace();
        if let Some(server) = words.next() {
            options.server = Some(server.to_string());
        }
        while let Some(flag) = words.next() {
            match flag {
                "-l" => {
                    options.login_timeout = words
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(options.login_timeout)
                }
                "-U" => options.user = words.next().map(str::to_string),
                "-P" => options.password = words.next().map(str::to_string),
                _ => {}
            }
        }

        let workstation = hostname();
        let (context, source) = connect::build_context(&options, &workstation);
        match connect::connect(context, &source, Some(&self.cancel)).await {
            Ok(client) => {
                if let Some(old) = &mut self.client {
                    let _ = old.close_connection().await;
                }
                if let Some(server) = &options.server {
                    self.vars.set_internal("SQLCMDSERVER", server);
                }
                self.vars.set_internal("SQLCMDDBNAME", client.database());
                if let Some(user) = &options.user {
                    self.vars.set_internal("SQLCMDUSER", user);
                }
                self.client = Some(client);
                self.options = options;
            }
            Err(error) => self.errors.write(&connection_error_text(
                &error,
                self.options.raw_error_messages,
            )),
        }
    }

    fn shell(&mut self, line: &str) {
        if self.options.disable_commands || self.options.disable_commands_and_exit {
            return;
        }
        let status = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", line])
                .status()
        } else {
            std::process::Command::new("sh").args(["-c", line]).status()
        };
        if status.is_err() {
            self.errors.write(&messages::command_syntax_error("!!"));
        }
    }

    async fn exit(&mut self, form: ExitForm) -> Option<Stop> {
        match form {
            ExitForm::Immediate => Some(Stop::Requested(self.final_exit_code())),
            ExitForm::RunCache => {
                if !self.batch.is_empty() {
                    self.execute_cache(1).await;
                }
                Some(Stop::Requested(self.final_exit_code()))
            }
            ExitForm::Query(query) => {
                if !self.batch.is_empty() {
                    self.execute_cache(1).await;
                }
                let code = self.query_exit_code(&query).await;
                Some(Stop::Requested(code))
            }
        }
    }

    /// `:exit(query)` runs the query, prints its result like any other, and
    /// takes the first cell of the first row as the exit code.
    async fn query_exit_code(&mut self, query: &str) -> i32 {
        use mssql_tds::datatypes::column_values::ColumnValues;

        let style = self.run_style();
        let timeout = self.options.query_timeout_option();
        let cancel = self.cancel.child_handle();
        let Some(client) = &mut self.client else {
            return exitcode::NO_RESULT;
        };

        let outcome = runner::run(client, query, timeout, Some(&cancel), &style).await;
        let first = outcome.first_cell.clone();
        let failed = outcome.failed;
        self.emit(outcome);

        if failed {
            return exitcode::NO_RESULT;
        }
        match first {
            Some(ColumnValues::Int(v)) => v,
            Some(ColumnValues::BigInt(v)) => v as i32,
            Some(ColumnValues::SmallInt(v)) => v as i32,
            Some(ColumnValues::TinyInt(v)) => v as i32,
            Some(ColumnValues::Bit(b)) => i32::from(b),
            Some(ColumnValues::Null) => exitcode::NOT_NUMERIC,
            Some(_) => exitcode::NOT_NUMERIC,
            None => exitcode::NO_ROWS,
        }
    }

    async fn execute_cache(&mut self, count: u32) {
        let substitute_vars = !self.options.disable_variable_substitution;
        let expansion = self.batch.resolve(&self.vars, substitute_vars);
        for name in &expansion.undefined {
            self.errors.write(&messages::var_not_defined(name));
        }
        let text = expansion.text;
        self.batch.reset();

        if text.trim().is_empty() {
            return;
        }

        for _ in 0..count {
            self.run_text(&text, 1).await;
            if self.highest_severity > self.options.error_level as i32
                && matches!(self.on_error, OnError::Exit)
            {
                break;
            }
        }
    }

    async fn run_text(&mut self, sql: &str, _repeat: u32) {
        let style = self.run_style();
        let timeout = self.options.query_timeout_option();
        let cancel = self.cancel.child_handle();
        let Some(client) = &mut self.client else {
            return;
        };

        // Ctrl+C sends an ATTENTION rather than killing the process, so the
        // connection survives and the prompt comes back.
        let interrupted;
        let outcome = {
            let signal = cancel.child_handle();
            let query = runner::run(client, sql, timeout, Some(&cancel), &style);
            tokio::pin!(query);
            tokio::select! {
                outcome = &mut query => {
                    interrupted = false;
                    outcome
                }
                _ = tokio::signal::ctrl_c() => {
                    signal.cancel();
                    interrupted = true;
                    query.await
                }
            }
        };

        tracing::write(&format!(
            "batch: {} ms, highest severity {}, {} bytes of sql",
            outcome.elapsed_ms,
            outcome.highest_severity,
            sql.len()
        ));

        if interrupted {
            self.errors.write(&messages::user_terminated());
        }
        self.emit(outcome);
    }

    /// Writes an outcome out, sending each server message to whichever stream
    /// `-r` selected and dropping the ones `-m` filters away.
    fn emit(&mut self, outcome: runner::Outcome) {
        if outcome.highest_severity > self.highest_severity {
            self.highest_severity = outcome.highest_severity;
        }

        let route_errors = self.options.errors_to_stderr;
        let raw = self.options.raw_error_messages;
        let threshold = self.options.error_level;
        let go = self.options.compat.is_go();

        for item in outcome.output {
            match item {
                Output::Result(text) => self.results.write(&text),
                Output::Message(message) => {
                    // `-m n` hides anything below severity n. ODBC applies the
                    // threshold as given; go-sqlcmd never hides `PRINT` output
                    // and other severity-10 chatter, whatever `-m` says.
                    let hidden = threshold >= 0 && (message.severity as i64) < threshold;
                    if hidden && (!go || message.is_error()) {
                        continue;
                    }
                    let to_stderr = match route_errors {
                        // `-r` absent: everything goes to the results stream.
                        n if n < 0 => false,
                        // `-r0`: errors only. `-r1`: errors and informational.
                        0 => message.is_error(),
                        _ => true,
                    };
                    // go-sqlcmd drops the `Msg ...` header once a message has
                    // been routed to stderr, and follows it with a blank line.
                    let rendered = if to_stderr && go {
                        format!("{}{EOL}{EOL}", message.text)
                    } else {
                        message.render(raw)
                    };
                    // A message above severity 10 is an error; the rest,
                    // including `PRINT`, are drawn as warnings.
                    let rendered = self.colors.paint_lines(
                        &rendered,
                        if message.is_error() {
                            TextType::Error
                        } else {
                            TextType::Warning
                        },
                    );
                    if to_stderr {
                        self.errors.write(&rendered);
                    } else {
                        self.results.write(&rendered);
                    }
                }
            }
        }

        if self.options.print_statistics {
            let stats = if self.options.statistics_colon_format {
                report::perf_stats_colon(outcome.packet_size, 1, outcome.elapsed_ms)
            } else {
                report::perf_stats(outcome.packet_size, 1, outcome.elapsed_ms)
            };
            self.results.write(&stats);
        }

        self.results.flush();
        self.errors.flush();
    }

    fn run_style(&self) -> RunStyle {
        RunStyle {
            table: TableStyle {
                separator: self.options.column_separator.clone(),
                headers: self.vars.get_int("SQLCMDHEADERS", self.options.headers),
                screen_width: usize::try_from(
                    self.vars
                        .get_int("SQLCMDCOLWIDTH", self.options.column_width),
                )
                .unwrap_or(0),
                trim: self.options.trim_columns,
                control_chars: self.options.control_chars,
                gap_before_repeat: self.options.compat.is_go(),
                colors: self.colors,
            },
            max_var_width: usize::try_from(
                self.vars
                    .get_int("SQLCMDMAXVARTYPEWIDTH", self.options.var_type_width),
            )
            .unwrap_or(0),
            max_fixed_width: usize::try_from(
                self.vars
                    .get_int("SQLCMDMAXFIXEDTYPEWIDTH", self.options.fixed_type_width),
            )
            .unwrap_or(0),
            xml: self.xml_mode,
            format: self.format,
            compat: self.options.compat,
            colors: self.colors,
        }
    }
}

/// The driver name the reference attributes connection failures to.
const DRIVER_NAME: &str = "Microsoft ODBC Driver 18 for SQL Server";

/// `-u` asks for UTF-16LE; `-f o:<cp>` names a code page; otherwise UTF-8.
fn output_encoding(options: &Options) -> OutputEncoding {
    if options.unicode_output {
        return OutputEncoding::Utf16Le;
    }
    match options.output_code_page.and_then(encoding_for_code_page) {
        Some(encoding) => OutputEncoding::CodePage(encoding),
        None => OutputEncoding::Utf8,
    }
}

/// Reads a password from the console without echoing it.
fn prompt_for_password() -> Option<String> {
    use std::io::{IsTerminal, Write};

    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return None;
    }
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(b"Password: ");
    let _ = stderr.flush();

    let password = read_hidden_line();
    let _ = stderr.write_all(EOL.as_bytes());
    password
}

#[cfg(windows)]
fn read_hidden_line() -> Option<String> {
    // The console is put into no-echo mode for the duration of the read so the
    // password never reaches the screen.
    use std::io::BufRead;

    let previous = console_mode();
    if let Some(mode) = previous {
        set_console_mode(mode & !0x0004); // ENABLE_ECHO_INPUT
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line).ok();
    if let Some(mode) = previous {
        set_console_mode(mode);
    }
    read.map(|_| line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(windows))]
fn read_hidden_line() -> Option<String> {
    use std::io::BufRead;

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .ok()
        .map(|_| line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(windows)]
fn console_mode() -> Option<u32> {
    unsafe extern "system" {
        fn GetStdHandle(n: i32) -> isize;
        fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
    }
    let mut mode = 0u32;
    // SAFETY: both calls take a handle owned by the process and a pointer to a
    // local, and their success is checked before the value is used.
    unsafe {
        let handle = GetStdHandle(-10); // STD_INPUT_HANDLE
        (GetConsoleMode(handle, &mut mode) != 0).then_some(mode)
    }
}

#[cfg(windows)]
fn set_console_mode(mode: u32) {
    unsafe extern "system" {
        fn GetStdHandle(n: i32) -> isize;
        fn SetConsoleMode(handle: isize, mode: u32) -> i32;
    }
    // SAFETY: the handle is owned by the process and the mode is one previously
    // read from it.
    unsafe {
        let handle = GetStdHandle(-10);
        SetConsoleMode(handle, mode);
    }
}

/// A failed login can carry several server diagnostics; the reference prints
/// one line each, most specific first, which is the reverse of arrival order.
fn connection_error_text(error: &mssql_tds::error::Error, raw: bool) -> String {
    let decorate = |text: &str| {
        if raw {
            format!("[Microsoft][Rust Driver for SQL Server][SQL Server]{text}")
        } else {
            text.to_string()
        }
    };

    if let mssql_tds::error::Error::SqlServerError { diagnostics } = error {
        let mut out = String::new();
        for server_error in diagnostics.errors.iter().rev() {
            out.push_str(&messages::basic_errorinfo(
                DRIVER_NAME,
                &decorate(&server_error.message),
            ));
        }
        if !out.is_empty() {
            return out;
        }
    }
    messages::basic_errorinfo(DRIVER_NAME, &decorate(&error.to_string()))
}

/// `-X1` refuses the commands that reach outside the script.
fn is_disabled(text: &str) -> bool {
    let lowered = text.trim().to_ascii_lowercase();
    lowered.starts_with("ed") || lowered.starts_with("!!") || lowered.starts_with("connect")
}

fn seed_from_options(vars: &mut Variables, options: &Options, workstation: &str) {
    if let Some(server) = &options.server {
        vars.set_internal("SQLCMDSERVER", server);
    }
    match &options.user {
        Some(user) => vars.set_internal("SQLCMDUSER", user),
        // Under integrated auth go-sqlcmd reports the OS account it will
        // authenticate as; ODBC leaves the variable empty.
        None if options.compat.is_go() => {
            if let Some(account) = os_account() {
                vars.set_internal("SQLCMDUSER", &account);
            }
        }
        None => {}
    }
    if let Some(database) = &options.database {
        vars.set_internal("SQLCMDDBNAME", database);
    }
    vars.set_internal(
        "SQLCMDWORKSTATION",
        options.workstation.as_deref().unwrap_or(workstation),
    );
    vars.set_internal("SQLCMDLOGINTIMEOUT", &options.login_timeout.to_string());
    vars.set_internal("SQLCMDSTATTIMEOUT", &options.query_timeout.to_string());
    vars.set_internal("SQLCMDHEADERS", &options.headers.to_string());
    vars.set_internal("SQLCMDCOLSEP", &options.column_separator);
    vars.set_internal("SQLCMDCOLWIDTH", &options.column_width.to_string());
    vars.set_internal("SQLCMDPACKETSIZE", &options.packet_size.to_string());
    vars.set_internal("SQLCMDERRORLEVEL", &options.error_level.to_string());
    vars.set_internal("SQLCMDMAXVARTYPEWIDTH", &options.var_type_width.to_string());
    vars.set_internal(
        "SQLCMDMAXFIXEDTYPEWIDTH",
        &options.fixed_type_width.to_string(),
    );

    // `-v` assignments arrive as `NAME=VALUE` and override everything else.
    for assignment in &options.variables {
        if let Some((name, value)) = assignment.split_once('=') {
            let _ = vars.set(name, value);
        }
    }
}

/// Converts bare newlines to the CRLF the reference emits.
fn crlf(text: &str) -> String {
    text.replace(EOL, "\n").replace('\n', EOL)
}

/// The workstation name, as the reference reports it in `SQLCMDWORKSTATION`
/// and sends as the login's host name.
///
/// `COMPUTERNAME` is set on Windows, but `HOSTNAME` is a shell variable rather
/// than an exported one, so on Unix the name has to come from the system call.
fn hostname() -> String {
    if let Ok(name) = std::env::var("COMPUTERNAME")
        && !name.is_empty()
    {
        return name;
    }
    #[cfg(unix)]
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string())
}

/// The account integrated authentication will present, in the `DOMAIN\user`
/// form go-sqlcmd shows.
fn os_account() -> Option<String> {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Some(format!("{domain}\\{user}")),
        _ => Some(user),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_newlines_become_crlf_without_doubling_existing_ones() {
        assert_eq!(crlf("a\nb"), format!("a{EOL}b"));
        assert_eq!(crlf(&format!("a{EOL}b")), format!("a{EOL}b"));
    }

    #[test]
    fn x1_refuses_the_commands_that_reach_outside_the_script() {
        assert!(is_disabled("ed"));
        assert!(is_disabled("!! dir"));
        assert!(is_disabled("connect other"));
        assert!(!is_disabled("listvar"));
    }

    #[test]
    fn dash_v_assignments_are_split_on_the_first_equals() {
        let mut vars = Variables::default();
        let options = Options {
            variables: vec!["A=1".into(), "B=x=y".into()],
            ..Options::default()
        };
        seed_from_options(&mut vars, &options, "HOST");
        assert_eq!(vars.get("A"), Some("1"));
        assert_eq!(vars.get("B"), Some("x=y"));
    }
}

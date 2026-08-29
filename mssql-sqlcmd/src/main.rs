// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `sqlcmd` — a Rust implementation of the SQL Server command line tool.
//!
//! Command-line compatibility with the ODBC `sqlcmd` is a hard requirement, so
//! option grammar, diagnostics, output layout and exit codes are verified
//! against the shipped binary by the differential tests in `tests/diff.rs`.

mod batch;
mod cli;
mod commands;
mod compat;
mod dsn;
mod exec;
mod exitcode;
mod fmt;
mod io;
mod messages;
mod modern;
mod servers;
mod session;
mod tracing;
mod vars;

use std::io::Write;

use cli::args::{self, CliError};

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();

    // go-sqlcmd's subcommand CLI lives alongside the flag-driven one and is
    // told apart by the first argument alone.
    if modern::claims(&argv) {
        match modern::run(&argv) {
            Ok(modern::Outcome::Done(text)) => {
                print!("{text}");
                return;
            }
            // `query` resolves the current context into ordinary arguments and
            // runs through the machinery below.
            Ok(modern::Outcome::Delegate(resolved)) => argv = resolved,
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(exitcode::FAILURE);
            }
        }
    }

    let options = match parse(&argv) {
        Ok(Some(options)) => options,
        Ok(None) => return,
        Err(e) => {
            emit(&e);
            std::process::exit(exitcode::FAILURE);
        }
    };

    if let Err(e) = tracing::start(&options) {
        write_to(&mut std::io::stderr(), &e);
        std::process::exit(exitcode::FAILURE);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            write_to(
                &mut std::io::stderr(),
                &messages::basic_errorinfo("Sqlcmd", &e.to_string()),
            );
            std::process::exit(exitcode::FAILURE);
        }
    };

    let code = runtime.block_on(async {
        match session::Session::new(options) {
            Ok(session) => Box::pin(session.run()).await,
            Err(e) => {
                write_to(
                    &mut std::io::stderr(),
                    &messages::basic_errorinfo("Sqlcmd", &e.to_string()),
                );
                exitcode::FAILURE
            }
        }
    });

    // `:exit(query)` can ask for a value outside a byte, and the reference
    // passes it through unchanged, so bypass `ExitCode`.
    std::process::exit(code);
}

/// `Ok(None)` means the command line asked for something already satisfied,
/// such as `-?`.
fn parse(argv: &[String]) -> Result<Option<cli::validate::Options>, CliError> {
    let lexed = args::lex(argv)?;

    for warning in &lexed.warnings {
        write_to(&mut std::io::stderr(), warning);
    }

    if lexed.contains('?') {
        write_to(&mut std::io::stdout(), &cli::usage::usage());
        return Ok(None);
    }

    if lexed.contains(cli::spec::VERSION) {
        write_to(&mut std::io::stdout(), &cli::usage::banner());
        return Ok(None);
    }

    let mut options = cli::validate::resolve(&lexed)?;

    // A DSN fills in whatever the command line left unset.
    if let Some(name) = options.dsn.clone() {
        match dsn::load(&name) {
            Some(dsn) => dsn.apply_to(&mut options),
            None => {
                return Err(CliError::Stderr(messages::basic_errorinfo(
                    "Sqlcmd",
                    &format!("Data source name not found: '{name}'"),
                )));
            }
        }
    }

    Ok(Some(options))
}

fn emit(e: &CliError) {
    let text = e.to_string();
    if e.stream_is_stdout() {
        write_to(&mut std::io::stdout(), &text);
    } else {
        write_to(&mut std::io::stderr(), &text);
    }
}

/// Writes raw bytes so the CRLF endings the reference emits survive on every
/// platform, and ignores a closed pipe the way a console tool should.
fn write_to(sink: &mut impl Write, text: &str) {
    let _ = sink.write_all(text.as_bytes());
    let _ = sink.flush();
}

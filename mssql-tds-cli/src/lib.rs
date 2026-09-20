// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `sqlcmd` as a library, so the same implementation can be a standalone
//! binary or be linked into the native ODBC `sqlcmd`.
//!
//! [`run`] is the whole tool: it takes the arguments `main` would have read and
//! returns the process exit code instead of exiting, which is what lets a
//! caller written in C++ stay in control. [`ffi`] wraps it in a C ABI.

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

pub mod ffi;

use std::io::Write;

use cli::args::{self, CliError};

/// Runs sqlcmd over `argv`, which excludes the program name, and returns the
/// exit code.
///
/// Nothing here exits the process: the caller decides. That matters when the
/// caller is the native binary, which owns the process.
pub fn run(mut argv: Vec<String>) -> i32 {
    // go-sqlcmd's subcommand CLI lives alongside the flag-driven one and is
    // told apart by the first argument alone.
    if modern::claims(&argv) {
        match modern::run(&argv) {
            Ok(modern::Outcome::Done(text)) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                return exitcode::SUCCESS;
            }
            // `query` resolves the current context into ordinary arguments and
            // runs through the machinery below.
            Ok(modern::Outcome::Delegate(resolved)) => argv = resolved,
            Err(message) => {
                eprintln!("{message}");
                return exitcode::FAILURE;
            }
        }
    }

    let options = match parse(&argv) {
        Ok(Some(options)) => options,
        Ok(None) => return exitcode::SUCCESS,
        Err(e) => {
            emit(&e);
            return exitcode::FAILURE;
        }
    };

    if let Err(e) = tracing::start(&options) {
        write_to(&mut std::io::stderr(), &e);
        return exitcode::FAILURE;
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
            return exitcode::FAILURE;
        }
    };

    runtime.block_on(async {
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
    })
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

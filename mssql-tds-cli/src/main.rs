// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Error;

use clap::Parser;

use mssql_tds::core::EncryptionOptions;
use mssql_tds::core::EncryptionSetting;
use rustyline::Helper;
use rustyline::Result as RustylineResult;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{CompletionType, Config, Editor, error::ReadlineError};

use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
use mssql_tds::core::TdsResult;

#[tokio::main]
async fn main() {
    if let Err(e) = Box::pin(main_cli()).await {
        eprintln!("Application error: {e}");
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    config_file_path: String,
}

/// Define commands for auto-completion
const COMMANDS: &[&str] = &[
    "database connect",
    "query execute",
    "transaction begin",
    "transaction end",
    "query result iterate",
    "query result metadata get",
];

/// Custom Helper for Rustyline
struct MyHelper {}

impl Helper for MyHelper {}
impl Completer for MyHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> RustylineResult<(usize, Vec<Pair>)> {
        let mut matches = Vec::new();
        let start = line[..pos].rfind(' ').map_or(0, |pos| pos + 1);

        for &cmd in COMMANDS {
            if cmd.starts_with(&line[start..pos]) {
                matches.push(Pair {
                    display: cmd.to_string(),
                    replacement: cmd.to_string(),
                });
            }
        }

        Ok((start, matches))
    }
}
impl Hinter for MyHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        None // No inline hints for now
    }
}
impl Highlighter for MyHelper {}
impl Validator for MyHelper {
    fn validate(&self, _ctx: &mut ValidationContext) -> RustylineResult<ValidationResult> {
        Ok(ValidationResult::Valid(None))
    }
}

struct Session {
    connection: Option<Box<TdsClient>>,
}

#[allow(clippy::derivable_impls)]
impl Default for Session {
    fn default() -> Self {
        Self { connection: None }
    }
}

impl From<Box<TdsClient>> for Session {
    fn from(connection: Box<TdsClient>) -> Self {
        Self {
            connection: Some(connection),
        }
    }
}

impl Session {
    pub async fn submit_sql_batch(&mut self, sql_command: String) -> TdsResult<()> {
        let connection = self.connection.as_mut().ok_or(Error::new(
            std::io::ErrorKind::NotConnected,
            "No active connection",
        ))?;

        // Execute the SQL batch
        connection.execute(sql_command, None, None).await?;

        // Iterate through all result sets
        loop {
            // Check if there's a current result set
            if let Some(resultset) = connection.get_current_resultset() {
                // Read all rows from this result set
                let mut row_count = 0;
                while let Some(row) = resultset.next_row().await? {
                    row_count += 1;
                    // Print each column value
                    for value in row.iter() {
                        print!("{value:?} | ");
                    }
                    println!();
                }

                if row_count == 0 {
                    println!("(0 rows affected)");
                } else {
                    println!("({row_count} rows affected)");
                }
            } else {
                // No result set means DML operation
                println!("Command completed successfully");
            }

            // Try to move to the next result set
            if !connection.move_to_next().await? {
                break; // No more result sets
            }
        }
        Ok(())
    }
}

pub async fn main_cli() -> Result<(), Box<dyn std::error::Error>> {
    use mssql_tds::{
        connection::client_context::ClientContext,
        connection_provider::tds_connection_provider::TdsConnectionProvider,
    };
    let config = Config::builder()
        .completion_type(CompletionType::List) // Show list of completions
        .build();

    let mut rl = Editor::with_config(config)?;
    let helper = MyHelper {};
    rl.set_helper(Some(helper));

    let mut context = ClientContext::default();
    context.user_name = "sa".to_string();
    context.password = std::fs::read_to_string("/tmp/password")?.trim().to_string();
    context.database = "master".to_string();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::On,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };
    let provider = TdsConnectionProvider {};
    let datasource = "tcp:localhost,1433";
    let connection_result = provider.create_client(context, datasource, None).await;
    let mut session = match connection_result {
        Ok(_connection) => {
            println!("Successfully connected");
            Ok(Session::from(Box::new(_connection)))
        }
        Err(error) => {
            println!("Error: {:?}", error.to_string());
            Err(error)
        }
    };

    println!("Enter your SQL commands. Type 'exit' to quit.");

    loop {
        // Read user input with autocompletion enabled
        let readline = rl.readline("mssql-tds> ");
        match readline {
            Ok(line) => {
                rl.add_history_entry(&line)?;
                let trimmed_command = line.trim();
                if trimmed_command.eq_ignore_ascii_case("exit") {
                    println!("Exiting...");
                    break;
                }

                let session = session
                    .as_mut()
                    .map_err(|error| Error::other(format!("No active session: {error}")))?;
                session
                    .submit_sql_batch(trimmed_command.to_string())
                    .await?;
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                println!("Exiting...");
                break;
            }
            Err(err) => {
                println!("Error: {err:?}");
                break;
            }
        }
    }

    Ok(())
}

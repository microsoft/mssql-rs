// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `sqlcmd config …` — the commands that only touch the local file.
//!
//! None of these connect anywhere, so all of them are exercised by the
//! differential tests against the real go-sqlcmd.

use super::sqlconfig::{Context as ConfigContext, DEFAULT_PORT, Endpoint, Kind, User};
use super::yaml::{self, Yaml};
use super::{Context, Invocation};

pub const HELP: &str = "\
Modify sqlconfig files using subcommands like \"sqlcmd config use-context mssql\"

Usage:
  sqlcmd config [command]

Available Commands:
  add-context         Add a context
  add-endpoint        Add an endpoint
  add-user            Add a user
  connection-strings  Display connection strings for the current context
  current-context     Display the current context
  delete-context      Delete a context
  delete-endpoint     Delete an endpoint
  delete-user         Delete a user
  get-contexts        Display one or many contexts
  get-endpoints       Display one or many endpoints
  get-users           Display one or many users
  use-context         Set the current context
  view                Display merged sqlconfig settings

Flags:
      --sqlconfig string   Configuration file
      --help               help for config
";

pub fn run(invocation: &mut Invocation, mut context: Context) -> Result<String, String> {
    let subcommand = invocation.take_word().unwrap_or_default();
    match subcommand.as_str() {
        "add-endpoint" => add_endpoint(invocation, &mut context),
        "add-user" => add_user(invocation, &mut context),
        "add-context" => add_context(invocation, &mut context),
        "get-contexts" => Ok(get_contexts(invocation, &context)),
        "get-endpoints" => Ok(get_endpoints(invocation, &context)),
        "get-users" => Ok(get_users(invocation, &context)),
        "current-context" => current_context(&context),
        "use-context" | "use" | "change-context" | "set-context" => {
            use_context(invocation, &mut context)
        }
        "delete-context" => delete_context(invocation, &mut context),
        "delete-endpoint" => delete_endpoint(invocation, &mut context),
        "delete-user" => delete_user(invocation, &mut context),
        "view" | "show" => Ok(view(invocation, &context)),
        "connection-strings" | "cs" => connection_strings(invocation, &context),
        "" => Ok(HELP.to_string()),
        other => Err(format!("unknown command {other:?} for \"sqlcmd config\"")),
    }
}

fn add_endpoint(invocation: &mut Invocation, context: &mut Context) -> Result<String, String> {
    let name = context
        .config
        .unique_name(invocation.flag_or("name", "endpoint"), Kind::Endpoint);
    let address = invocation.flag_or("address", "localhost").to_string();
    let port = invocation.number("port", DEFAULT_PORT)?;

    context.config.endpoints.push(Endpoint {
        name: name.clone(),
        address: address.clone(),
        port,
        container: None,
    });
    context.save()?;

    Ok(format!(
        "Endpoint '{name}' added (address: '{address}', port: '{}')\n\n{}",
        thousands(port),
        hints(&[
            (
                "Add a context for this endpoint",
                &format!("sqlcmd config add-context --endpoint {name}")
            ),
            ("View endpoint names", "sqlcmd config get-endpoints"),
            (
                "View endpoint details",
                &format!("sqlcmd config get-endpoints {name}")
            ),
            (
                "View all endpoints details",
                "sqlcmd config get-endpoints --detailed"
            ),
            (
                "Delete this endpoint",
                &format!("sqlcmd config delete-endpoint {name}")
            ),
        ])
    ))
}

fn add_user(invocation: &mut Invocation, context: &mut Context) -> Result<String, String> {
    let name = context
        .config
        .unique_name(invocation.flag_or("name", "user"), Kind::User);
    let authentication_type = invocation.flag_or("auth-type", "basic").to_string();
    if !matches!(authentication_type.as_str(), "basic" | "other") {
        return Err(failure_plain(
            &["Use --auth-type basic or --auth-type other"],
            &format!("Authentication type '{authentication_type}' is not valid"),
        ));
    }

    let encryption = invocation.flag("password-encryption").unwrap_or_default();
    if authentication_type == "basic" && encryption.is_empty() {
        return Err(failure_plain(
            &["Add the --password-encryption flag"],
            "The --password-encryption flag must be set when authentication type is 'basic'",
        ));
    }

    // The password never appears on the command line, where it would reach the
    // shell history and the process table.
    let password = std::env::var("SQLCMD_PASSWORD")
        .or_else(|_| std::env::var("SQLCMDPASSWORD"))
        .unwrap_or_default();
    if authentication_type == "basic" && password.is_empty() {
        return Err(failure_plain(
            &["Provide password in the SQLCMD_PASSWORD (or SQLCMDPASSWORD) environment variable"],
            "Authentication Type 'basic' requires a password",
        ));
    }

    context.config.users.push(User {
        name: name.clone(),
        authentication_type,
        username: invocation.flag_or("username", "").to_string(),
        password: base64::encode(password.as_bytes()),
        password_encryption: encryption.to_string(),
    });
    context.save()?;
    Ok(format!("User '{name}' added\n"))
}

fn add_context(invocation: &mut Invocation, context: &mut Context) -> Result<String, String> {
    let endpoint = invocation.flag("endpoint").unwrap_or_default().to_string();
    if endpoint.is_empty() || context.config.find_endpoint(&endpoint).is_none() {
        return Err(failure(
            &[
                (
                    "View existing endpoints to choose from",
                    "sqlcmd config get-endpoints",
                ),
                ("Add a new local endpoint", "sqlcmd create"),
                (
                    "Add an already existing endpoint",
                    "sqlcmd config add-endpoint --address localhost --port 1433",
                ),
            ],
            &format!(
                "Endpoint required to add context.  Endpoint '{endpoint}' does not exist.  Use --endpoint flag"
            ),
        ));
    }
    let user = invocation.flag("user").map(str::to_string);
    if let Some(user) = &user
        && context.config.find_user(user).is_none()
    {
        let add = format!("sqlcmd config add-user --name {user}");
        return Err(failure(
            &[
                ("View list of users", "sqlcmd config get-users"),
                ("Add the user", &add),
                ("Add an endpoint", "sqlcmd create"),
            ],
            &format!("User '{user}' does not exist"),
        ));
    }

    let name = context
        .config
        .unique_name(invocation.flag_or("name", "context"), Kind::Context);
    context.config.contexts.push(ConfigContext {
        name: name.clone(),
        endpoint,
        user,
    });
    context.config.current_context = name.clone();
    context.save()?;

    Ok(format!(
        "Current Context '{name}'\n\n{}",
        hints(&[
            ("Open in Azure Data Studio", "sqlcmd open ads"),
            ("To start interactive query session", "sqlcmd query"),
            ("To run a query", "sqlcmd query \"SELECT @@version\""),
        ])
    ))
}

/// `get-*` prints bare names, or full detail under `--detailed`. Naming one
/// entry prints that entry's map on its own, without the list marker.
fn listing(
    invocation: &mut Invocation,
    names: Vec<String>,
    detail: impl Fn(&str) -> Option<Yaml>,
) -> String {
    let detailed = invocation.has("detailed");
    if let Some(wanted) = invocation.name_argument() {
        return match detail(&wanted) {
            Some(entry) => yaml::emit(&entry),
            None => yaml::emit(&Yaml::List(Vec::new())),
        };
    }
    if detailed {
        let entries: Vec<Yaml> = names.iter().filter_map(|n| detail(n)).collect();
        yaml::emit(&Yaml::List(entries))
    } else {
        yaml::emit(&Yaml::List(names.into_iter().map(Yaml::Scalar).collect()))
    }
}

fn get_contexts(invocation: &mut Invocation, context: &Context) -> String {
    let names = context
        .config
        .contexts
        .iter()
        .map(|c| c.name.clone())
        .collect();
    listing(invocation, names, |name| {
        context.config.find_context(name).map(|c| {
            let mut detail = vec![("endpoint".to_string(), Yaml::scalar(&c.endpoint))];
            if let Some(user) = &c.user {
                detail.push(("user".to_string(), Yaml::scalar(user)));
            }
            Yaml::Map(vec![
                ("context".to_string(), Yaml::Map(detail)),
                ("name".to_string(), Yaml::scalar(&c.name)),
            ])
        })
    })
}

fn get_endpoints(invocation: &mut Invocation, context: &Context) -> String {
    let names = context
        .config
        .endpoints
        .iter()
        .map(|e| e.name.clone())
        .collect();
    listing(invocation, names, |name| {
        context.config.find_endpoint(name).map(|e| {
            Yaml::Map(vec![
                (
                    "endpoint".to_string(),
                    Yaml::Map(vec![
                        ("address".to_string(), Yaml::scalar(&e.address)),
                        ("port".to_string(), Yaml::scalar(e.port.to_string())),
                    ]),
                ),
                ("name".to_string(), Yaml::scalar(&e.name)),
            ])
        })
    })
}

fn get_users(invocation: &mut Invocation, context: &Context) -> String {
    let names = context
        .config
        .users
        .iter()
        .map(|u| u.name.clone())
        .collect();
    listing(invocation, names, |name| {
        context.config.find_user(name).map(|u| {
            Yaml::Map(vec![
                ("name".to_string(), Yaml::scalar(&u.name)),
                (
                    "authentication-type".to_string(),
                    Yaml::scalar(&u.authentication_type),
                ),
                (
                    "basic-auth".to_string(),
                    Yaml::Map(vec![
                        ("username".to_string(), Yaml::scalar(&u.username)),
                        (
                            "password-encryption".to_string(),
                            Yaml::scalar(&u.password_encryption),
                        ),
                        ("password".to_string(), Yaml::scalar(&u.password)),
                    ]),
                ),
            ])
        })
    })
}

/// The current context, or a blank line when there is none — go-sqlcmd prints
/// the empty name rather than treating it as an error.
fn current_context(context: &Context) -> Result<String, String> {
    Ok(format!("{}\n", context.config.current_context))
}

fn use_context(invocation: &mut Invocation, context: &mut Context) -> Result<String, String> {
    let name = invocation.name_argument().unwrap_or_default();
    if context.config.find_context(&name).is_none() {
        return Err(failure_plain(
            &["To view available contexts run `sqlcmd config get-contexts`"],
            &format!("No context exists with the name: \"{name}\""),
        ));
    }
    context.config.current_context = name.clone();
    context.save()?;
    Ok(format!(
        "Switched to context \"{name}\".\n\n{}",
        // This block is padded past its own labels, so the width is stated.
        hints_padded(
            &[
                ("To run a query", "sqlcmd query \"SELECT @@SERVERNAME\""),
                ("To remove", "sqlcmd uninstall"),
            ],
            17
        )
    ))
}

fn delete_context(invocation: &mut Invocation, context: &mut Context) -> Result<String, String> {
    let name = invocation.name_argument().unwrap_or_default();
    let Some(found) = context.config.find_context(&name).cloned() else {
        return Err(failure(
            &[("View available contexts", "sqlcmd config get-contexts")],
            &format!("Context '{name}' does not exist"),
        ));
    };

    // The endpoint and user exist to serve the context, so they go with it
    // unless `--cascade=false` says to keep them.
    let cascade = invocation.flag("cascade") != Some("false");
    if cascade {
        context
            .config
            .endpoints
            .retain(|e| e.name != found.endpoint);
        if let Some(user) = &found.user {
            context.config.users.retain(|u| &u.name != user);
        }
    }
    context.config.contexts.retain(|c| c.name != name);
    if context.config.current_context == name {
        context.config.current_context = String::new();
    }
    context.save()?;
    Ok(format!("Context '{name}' deleted\n"))
}

fn delete_endpoint(invocation: &mut Invocation, context: &mut Context) -> Result<String, String> {
    let name = invocation.name_argument().unwrap_or_default();
    if context.config.find_endpoint(&name).is_none() {
        return Err(failure(
            &[("View endpoints", "sqlcmd config get-endpoints")],
            &format!("Endpoint '{name}' does not exist"),
        ));
    }
    context.config.endpoints.retain(|e| e.name != name);
    context.save()?;
    Ok(format!("Endpoint '{name}' deleted\n"))
}

fn delete_user(invocation: &mut Invocation, context: &mut Context) -> Result<String, String> {
    let name = invocation.name_argument().unwrap_or_default();
    if context.config.find_user(&name).is_none() {
        return Err(failure(
            &[("View users", "sqlcmd config get-users")],
            &format!("User \"{name}\" does not exist"),
        ));
    }
    context.config.users.retain(|u| u.name != name);
    context.save()?;
    Ok(format!("User \"{name}\" deleted\n"))
}

fn view(invocation: &mut Invocation, context: &Context) -> String {
    yaml::emit(&context.config.to_yaml_view(invocation.has("raw")))
}

fn connection_strings(invocation: &mut Invocation, context: &Context) -> Result<String, String> {
    let Some((_, endpoint, user)) = context.config.current() else {
        return Err(no_context());
    };
    let database = invocation.flag_or("database", "master");
    let address = &endpoint.address;
    let port = endpoint.port;

    let (username, password) = match user {
        Some(user) => (user.username.clone(), base64::decode_text(&user.password)),
        None => (String::new(), String::new()),
    };

    // Integrated auth carries no credentials, so the fragments that name them
    // drop out entirely rather than appearing empty.
    let (odbc_auth, ado_auth, jdbc_auth) = if username.is_empty() {
        (
            "Trusted_Connection=yes;".to_string(),
            "Integrated Security=True;".to_string(),
            "integratedSecurity=true;".to_string(),
        )
    } else {
        (
            format!("Uid={username};Pwd={password};"),
            format!("User ID={username};Password={password};"),
            format!("user={username};password={password};"),
        )
    };
    let go_credentials = if username.is_empty() {
        String::new()
    } else {
        format!("{username}:{password}@")
    };
    let sqlcmd_line = if username.is_empty() {
        format!("sqlcmd -S {address},{port} -d {database}")
    } else if cfg!(windows) {
        format!(
            "SET \"SQLCMDPASSWORD={password}\" & sqlcmd -S {address},{port} -U {username} -d {database}"
        )
    } else {
        // The line is meant to be pasted into a shell, so it has to be the
        // shell the caller is actually running.
        format!(
            "export 'SQLCMDPASSWORD={password}'; sqlcmd -S {address},{port} -U {username} -d {database}"
        )
    };

    Ok(format!(
        "ADO.NET: Server=tcp:{address},{port};Initial Catalog={database};Persist Security Info=False;{ado_auth}MultipleActiveResultSets=False;Encrypt=True;TrustServerCertificate=True;Connection Timeout=30;\n\
         JDBC:    jdbc:sqlserver://{address}:{port};database={database};{jdbc_auth}encrypt=true;trustServerCertificate=true;loginTimeout=30;\n\
         ODBC:    Driver={{ODBC Driver 18 for SQL Server}};Server=tcp:{address},{port};Database={database};{odbc_auth}Encrypt=yes;TrustServerCertificate=yes;Connection Timeout=30;\n\
         GO:      sqlserver://{go_credentials}{address},{port}?database={database};encrypt=true;trustServerCertificate=true;dial+timeout=30\n\
         SQLCMD:  {sqlcmd_line}\n"
    ))
}

/// The wording `query` and `connection-strings` use when no context is set.
pub fn no_context() -> String {
    "Error: no current context. To create a context use `sqlcmd create`, e.g. `sqlcmd create mssql`"
        .to_string()
}

/// The wording `start`, `stop` and `delete` use for the same condition.
pub fn no_context_hint() -> String {
    failure(
        &[("To view available contexts", "sqlcmd config get-contexts")],
        "No current context",
    )
}

/// go-sqlcmd reports a failure as a hint block followed by an `Error:` line,
/// all on stderr. Hints given as `(label, command)` line their commands up the
/// same way the success blocks do.
fn failure(hints: &[(&str, &str)], error: &str) -> String {
    let mut out = String::from("\n");
    out.push_str(&hints_padded(hints, 0));
    out.push_str(&format!("\nError: {error}"));
    out
}

/// A failure whose hints are single phrases with no command column.
pub fn failure_plain(hints: &[&str], error: &str) -> String {
    let mut out = String::from("\nHINT:\n");
    for (index, hint) in hints.iter().enumerate() {
        out.push_str(&format!("  {}. {hint}\n", index + 1));
    }
    out.push_str(&format!("\nError: {error}"));
    out
}

/// Passwords are stored base64-encoded, which is obfuscation rather than
/// encryption — `--password-encryption none` means exactly that. The encoding
/// exists so a password containing YAML metacharacters survives the file.
pub mod base64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let triple = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for index in 0..4 {
                if index <= chunk.len() {
                    out.push(ALPHABET[((triple >> (18 - index * 6)) & 0x3F) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    pub fn decode(input: &str) -> Option<Vec<u8>> {
        let mut bits = Vec::with_capacity(input.len());
        for ch in input.bytes().filter(|c| *c != b'=') {
            bits.push(ALPHABET.iter().position(|a| *a == ch)? as u32);
        }
        let mut out = Vec::with_capacity(bits.len() * 3 / 4);
        for chunk in bits.chunks(4) {
            let mut packed = 0u32;
            for (index, value) in chunk.iter().enumerate() {
                packed |= value << (18 - index * 6);
            }
            for index in 0..chunk.len().saturating_sub(1) {
                out.push(((packed >> (16 - index * 8)) & 0xFF) as u8);
            }
        }
        Some(out)
    }

    /// The stored form as text, falling back to the raw string when it is not
    /// valid base64 — a hand-edited file should still be usable.
    pub fn decode_text(stored: &str) -> String {
        decode(stored)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_else(|| stored.to_string())
    }
}

/// The numbered hint block the reference prints after a command that changes
/// something. The commands line up one space past the longest label.
pub fn hints(items: &[(&str, &str)]) -> String {
    hints_padded(items, 0)
}

/// As [`hints`], but with a floor on the label column. A few blocks are padded
/// wider than their own labels need, so the width cannot always be derived.
fn hints_padded(items: &[(&str, &str)], floor: usize) -> String {
    let width = items
        .iter()
        .map(|(text, _)| text.len())
        .max()
        .unwrap_or(0)
        .max(floor);
    let mut out = String::from("HINT:\n");
    for (index, (text, command)) in items.iter().enumerate() {
        out.push_str(&format!(
            "  {}. {text}:{:pad$}{command}\n",
            index + 1,
            "",
            pad = width - text.len() + 1
        ));
    }
    out
}

/// go-sqlcmd formats the port with a thousands separator in this one message.
fn thousands(value: u16) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for text in ["", "a", "ab", "abc", "abcd", "Test-Pass-123!"] {
            let encoded = base64::encode(text.as_bytes());
            assert_eq!(base64::decode_text(&encoded), text, "for {text:?}");
        }
    }

    #[test]
    fn base64_matches_the_reference_encoding() {
        // The value go-sqlcmd wrote for this password.
        assert_eq!(base64::encode(b"Test-Pass-123!"), "VGVzdC1QYXNzLTEyMyE=");
    }

    #[test]
    fn a_password_that_is_not_base64_is_passed_through() {
        assert_eq!(base64::decode_text("not base64!!"), "not base64!!");
    }

    #[test]
    fn a_failure_leads_with_hints_and_ends_with_the_error() {
        assert_eq!(
            failure_plain(&["do this"], "it broke"),
            "\nHINT:\n  1. do this\n\nError: it broke"
        );
        assert_eq!(
            failure(&[("Do this", "a"), ("Or this longer one", "b")], "it broke"),
            "\nHINT:\n  1. Do this:            a\n  2. Or this longer one: b\n\nError: it broke"
        );
    }

    #[test]
    fn the_port_carries_a_thousands_separator() {
        assert_eq!(thousands(1433), "1,433");
        assert_eq!(thousands(433), "433");
        assert_eq!(thousands(14330), "14,330");
    }

    #[test]
    fn hints_line_up_their_commands() {
        let text = hints(&[("Short", "a"), ("Much longer text", "b")]);
        assert_eq!(
            text,
            "HINT:\n  1. Short:            a\n  2. Much longer text: b\n"
        );
    }

    #[test]
    fn a_padding_floor_widens_the_label_column() {
        // `use-context` pads wider than its own labels need.
        let text = hints_padded(&[("A", "x")], 5);
        assert_eq!(text, "HINT:\n  1. A:     x\n");
    }
}

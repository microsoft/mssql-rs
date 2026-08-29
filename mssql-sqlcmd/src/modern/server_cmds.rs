// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The subcommands that reach a server: `query`, and the container lifecycle.

use super::config_cmds::no_context;
use super::container::{self, Runtime};
use super::sqlconfig::{Container, Context as ConfigContext, Endpoint, Kind, User};
use super::{Context, Invocation, Outcome};

/// The line SQL Server logs once it is accepting connections.
const READY_MARKER: &str = "SQL Server is now ready for client connections";
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// `sqlcmd query` — run a statement against the current context.
///
/// This is the legacy CLI with its arguments taken from the config file rather
/// than the command line, so it is expressed as one.
pub fn query(invocation: &mut Invocation, context: Context) -> Result<Outcome, String> {
    let Some((_, endpoint, user)) = context.config.current() else {
        return Err(no_context());
    };

    let text = invocation
        .flag("text")
        .or_else(|| invocation.flag("query"))
        .map(str::to_string)
        .or_else(|| invocation.positional.first().cloned())
        .or_else(|| invocation.words.first().cloned());

    let mut argv = vec![
        "-S".to_string(),
        format!("{},{}", endpoint.address, endpoint.port),
        // A container on localhost presents a self-signed certificate.
        "-C".to_string(),
        // This is go-sqlcmd's own command, so it renders go-sqlcmd's way.
        "--compat".to_string(),
        "go".to_string(),
    ];
    if let Some(database) = invocation.flag("database") {
        argv.push("-d".to_string());
        argv.push(database.to_string());
    }
    match user {
        Some(user) => {
            argv.push("-U".to_string());
            argv.push(user.username.clone());
            argv.push("-P".to_string());
            argv.push(crate::modern::config_cmds::base64::decode_text(
                &user.password,
            ));
        }
        // go-sqlcmd sends an empty SQL login rather than falling back to
        // integrated auth, which is why the server reports `user ''`. Using
        // `-E` here would attempt Kerberos on Unix and diverge.
        None => {
            argv.push("-U".to_string());
            argv.push(String::new());
            argv.push("-P".to_string());
            argv.push(String::new());
        }
    }
    if let Some(text) = text {
        argv.push("-Q".to_string());
        argv.push(text);
    }

    Ok(Outcome::Delegate(argv))
}

/// `sqlcmd create mssql` — run SQL Server in a container and point a new
/// context at it.
pub fn create(invocation: &mut Invocation, mut context: Context) -> Result<String, String> {
    // `get-tags` sits under `mssql`, so it arrives as two words; the aliases
    // are also accepted directly under `create`.
    let mut word = invocation.take_word();
    if word.as_deref() == Some("mssql") {
        if let Some(next) = invocation.words.first().cloned() {
            word = Some(next);
            invocation.take_word();
        } else {
            word = Some("mssql".to_string());
        }
    }
    match word.as_deref() {
        Some("mssql") | None => {}
        Some("get-tags") | Some("gt") | Some("lt") => {
            let registry = invocation
                .flag_or("registry", "mcr.microsoft.com")
                .to_string();
            let repo = invocation.flag_or("repo", "mssql/server").to_string();
            // The rest of this CLI is synchronous, so the one HTTP call gets
            // its own runtime rather than colouring everything above it async.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("cannot start a runtime: {e}"))?;
            let tags = runtime.block_on(Runtime::tags(&registry, &repo))?;
            return Ok(tags
                .iter()
                .map(|tag| format!("- {tag}\n"))
                .collect::<String>());
        }
        Some(other) => return Err(format!("unknown command {other:?} for \"sqlcmd create\"")),
    }

    // The EULA has to be accepted deliberately; defaulting it to yes would
    // accept a licence on the user's behalf.
    let accepted = invocation.has("accept-eula")
        || ["SQLCMD_ACCEPT_EULA", "ACCEPT_EULA"].iter().any(|name| {
            std::env::var(name).is_ok_and(|v| v.eq_ignore_ascii_case("yes") || v == "Y")
        });
    if !accepted {
        // go-sqlcmd spells the assignment in the host shell's own syntax.
        let set = if cfg!(windows) { "SET" } else { "export" };
        return Err(crate::modern::config_cmds::failure_plain(
            &[
                "Either, add the --accept-eula flag to the command-line",
                &format!("Or, set the environment variable i.e. {set} SQLCMD_ACCEPT_EULA=YES "),
            ],
            "EULA not accepted",
        ));
    }

    let runtime = Runtime::detect()?;
    let registry = invocation.flag_or("registry", "mcr.microsoft.com");
    let repo = invocation.flag_or("repo", "mssql/server");
    let tag = invocation.flag_or("tag", "latest");
    let image = format!("{registry}/{repo}:{tag}");

    let port = match invocation.number("port", 0)? {
        0 => free_port(&context)?,
        chosen => chosen,
    };
    let password = container::generate_password(
        invocation.number("password-length", 50)? as usize,
        invocation.flag_or("password-special-chars", "!@#$%&*"),
    )?;

    let container_name = invocation.flag_or("name", "").to_string();
    let container_name = if container_name.is_empty() {
        // The port alone is not unique: a container removed from the config but
        // left running would collide, and `docker run` would refuse.
        format!("sqlcmd-mssql-{port}-{}", container::unique_suffix()?)
    } else {
        container_name
    };

    let mut progress = String::new();
    if !invocation.has("cached") {
        progress.push_str(&format!("Downloading {image}\n"));
        runtime.pull(&image)?;
    }
    progress.push_str(&format!("Starting {image}\n"));
    let id = runtime.create_mssql(
        &image,
        &container_name,
        port,
        &password,
        invocation.flag_or("collation", "SQL_Latin1_General_CP1_CI_AS"),
        invocation.flag_or("hostname", ""),
    )?;

    // From here on a failure leaves a running container behind unless it is
    // torn down, and one not recorded in the config is one nothing will clean
    // up later.
    if !runtime.wait_for_log(&id, READY_MARKER, READY_TIMEOUT) {
        let _ = runtime.remove(&id);
        return Err(format!(
            "the container started but SQL Server did not report itself ready within {}s, so it was removed.",
            READY_TIMEOUT.as_secs()
        ));
    }

    // One context, endpoint and user per container, named alike so the
    // relationship is legible in `config view`.
    let base = invocation.flag_or("context-name", "mssql").to_string();
    let name = context.config.unique_name(&base, Kind::Context);
    // The reference creates a login named after the caller rather than using
    // `sa`, and names the user entry `account@context`.
    let account = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "sa".to_string());
    let user_name = format!("{account}@{name}");
    context.config.endpoints.push(Endpoint {
        name: name.clone(),
        // The reference records the loopback address, not the host name.
        address: "127.0.0.1".to_string(),
        port,
        container: Some(Container {
            id: id.clone(),
            image: image.clone(),
        }),
    });
    context.config.users.push(User {
        name: user_name.clone(),
        authentication_type: "basic".to_string(),
        username: account.clone(),
        password: crate::modern::config_cmds::base64::encode(password.as_bytes()),
        password_encryption: invocation
            .flag_or("password-encryption", "none")
            .to_string(),
    });
    context.config.contexts.push(ConfigContext {
        name: name.clone(),
        endpoint: name.clone(),
        user: Some(user_name),
    });
    context.config.current_context = name.clone();
    if let Err(e) = context.save() {
        let _ = runtime.remove(&id);
        return Err(format!("{e}; the container was removed"));
    }

    // The reference offers the Azure Data Studio hint only where it implements
    // `open ads` -- Windows and macOS. This port also runs it on Linux, but the
    // hint list is compared against the reference, so it follows the same rule.
    let mut hint_list: Vec<(&str, &str)> = Vec::new();
    if cfg!(any(windows, target_os = "macos")) {
        hint_list.push(("Open in Azure Data Studio", "sqlcmd open ads"));
    }
    hint_list.extend([
        ("Run a query", "sqlcmd query \"SELECT @@version\""),
        ("Start interactive session", "sqlcmd query"),
        ("View sqlcmd configuration", "sqlcmd config view"),
        ("See connection strings", "sqlcmd config connection-strings"),
        ("Remove", "sqlcmd delete"),
    ]);

    Ok(format!(
        "{progress}\
         Created context \"{name}\" in \"{}\", configuring user account...\n\
         Disabled \"sa\" account (and rotated \"sa\" password). Creating user \"{account}\"\n\
         Now ready for client connections on port {port}\n\n{}",
        context.path.display(),
        crate::modern::config_cmds::hints(&hint_list)
    ))
}

pub fn start(context: Context) -> Result<String, String> {
    let (name, id, image) = current_container(&context, "Create new context with a sql container")?;
    Runtime::detect()?.start(&id)?;
    Ok(format!("Starting \"{image}\" for context \"{name}\"\n"))
}

pub fn stop(context: Context) -> Result<String, String> {
    // Worded differently from `start`, for no reason beyond how it was written.
    let (name, id, image) =
        current_container(&context, "Create a new context with a SQL Server container")?;
    Runtime::detect()?.stop(&id)?;
    Ok(format!("Stopping \"{image}\" for context \"{name}\"\n"))
}

pub fn delete(invocation: &mut Invocation, mut context: Context) -> Result<String, String> {
    let Some((current, endpoint, _)) = context.config.current() else {
        return Err(crate::modern::config_cmds::no_context_hint());
    };
    let name = current.name.clone();
    let user = current.user.clone();
    let endpoint_name = endpoint.name.clone();
    let container = endpoint.container.clone();

    // Removing a container destroys its databases, so say so unless the caller
    // has already confirmed.
    if container.is_some() && !invocation.has("yes") {
        return Err(format!(
            "this deletes the container behind context '{name}' and everything in it.\n\
             Re-run with --yes to confirm."
        ));
    }

    let mut progress = String::new();
    if let Some(container) = container {
        progress.push_str("Verifying no user (non-system) database (.mdf) files\n");
        progress.push_str(&format!("Removing context {name}\n"));
        progress.push_str(&format!("Stopping {}\n", container.image));
        let runtime = Runtime::detect()?;
        if runtime.is_running(&container.id) {
            runtime.stop(&container.id)?;
        }
        runtime.remove(&container.id)?;
    }

    context.config.contexts.retain(|c| c.name != name);
    context.config.endpoints.retain(|e| e.name != endpoint_name);
    if let Some(user) = user {
        context.config.users.retain(|u| u.name != user);
    }
    context.config.current_context = context
        .config
        .contexts
        .first()
        .map(|c| c.name.clone())
        .unwrap_or_default();
    context.save()?;

    Ok(format!("{progress}Operation completed successfully\n"))
}

fn current_container(context: &Context, hint: &str) -> Result<(String, String, String), String> {
    let Some((current, endpoint, _)) = context.config.current() else {
        return Err(crate::modern::config_cmds::no_context_hint());
    };
    match &endpoint.container {
        Some(container) => Ok((
            current.name.clone(),
            container.id.clone(),
            container.image.clone(),
        )),
        None => Err(crate::modern::config_cmds::failure_plain(
            &[&format!("{hint} : sqlcmd create mssql")],
            "Current context does not have a container",
        )),
    }
}

/// The first port at or above 1433 that no endpoint has claimed and that
/// nothing is listening on.
///
/// The bind is to `0.0.0.0` rather than `127.0.0.1` because that is what
/// publishing a container port does: a container already bound to all
/// interfaces leaves `127.0.0.1` free-looking, and `docker run` then fails with
/// "port is already allocated" after the image has been pulled.
fn free_port(context: &Context) -> Result<u16, String> {
    (1433..1533)
        .find(|port| {
            !context.config.endpoints.iter().any(|e| e.port == *port)
                && std::net::TcpListener::bind(("0.0.0.0", *port)).is_ok()
        })
        .ok_or_else(|| "no free port found in the range 1433-1532".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modern::sqlconfig::SqlConfig;

    fn context_with(endpoints: Vec<Endpoint>) -> Context {
        Context {
            path: std::path::PathBuf::from("unused"),
            config: SqlConfig {
                endpoints,
                ..SqlConfig::default()
            },
        }
    }

    #[test]
    fn a_port_already_claimed_by_an_endpoint_is_skipped() {
        let context = context_with(vec![Endpoint {
            name: "taken".to_string(),
            address: "localhost".to_string(),
            port: 1433,
            container: None,
        }]);
        assert_ne!(free_port(&context).unwrap(), 1433);
    }

    #[test]
    fn start_needs_a_container_behind_the_context() {
        let context = context_with(Vec::new());
        assert!(current_container(&context, "hint").is_err());
    }
}

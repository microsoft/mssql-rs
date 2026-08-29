// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Differential tests for the subcommand CLI against the real go-sqlcmd.
//!
//! Most cases touch only the local config file, so unlike the other harnesses
//! they need no server — just the reference binary named by `SQLCMD_DIFF_GO`.
//! Two kinds of case need more, and say so:
//!
//! - `needs_server` — `query`, which connects. Needs `SQLCMD_DIFF_CONNECT`.
//! - `needs_container` — `create`/`start`/`stop`/`delete`. Each pulls or starts
//!   an image, so a case costs minutes rather than milliseconds. Gated behind
//!   `SQLCMD_DIFF_CONTAINERS=1` and skipped otherwise, which keeps them out of
//!   a pre-push loop while leaving them available to a nightly run.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Path to the reference go-sqlcmd. Without it every case is skipped, so the
/// suite costs nothing on a machine without the Go toolchain.
const GO_ENV: &str = "SQLCMD_DIFF_GO";

/// Connection details for cases that reach a server. `query` takes its server
/// from the config file rather than the command line, so a `needs_server` case
/// builds the endpoint itself out of `{address}`, `{port}` and `{user}`, which
/// are substituted from the `-S` / `-U` / `-P` in this variable.
const CONNECT_ENV: &str = "SQLCMD_DIFF_CONNECT";

/// Set to `1` to run the container-lifecycle cases.
const CONTAINERS_ENV: &str = "SQLCMD_DIFF_CONTAINERS";

/// A password for `add-user`, which reads one from the environment rather than
/// the command line. Not a secret: it never leaves the test's config file.
const TEST_PASSWORD: &str = "Test-Pass-123!";

/// The address, port, user and password a `needs_server` case should use,
/// pulled out of `SQLCMD_DIFF_CONNECT`.
#[derive(Debug, Clone)]
struct Server {
    address: String,
    port: String,
    user: String,
    password: String,
}

fn server() -> Option<Server> {
    let connect = std::env::var(CONNECT_ENV).ok()?;
    let args: Vec<&str> = connect.split_whitespace().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| *a == flag)
            .and_then(|i| args.get(i + 1))
            .map(|s| s.to_string())
    };
    // `-S host,port` — the port is optional and defaults to SQL Server's.
    let target = value("-S").unwrap_or_else(|| "localhost".to_string());
    let (address, port) = match target.split_once(',') {
        Some((host, port)) => (host.to_string(), port.to_string()),
        None => (target, "1433".to_string()),
    };
    Some(Server {
        address,
        port,
        user: value("-U")?,
        password: value("-P")?,
    })
}

#[derive(Debug, serde::Deserialize)]
struct Case {
    name: String,
    steps: Vec<Vec<String>>,
    /// Set where the reference's line order varies between runs.
    #[serde(default)]
    unordered: bool,
    /// Set where a step connects to a server.
    #[serde(default)]
    needs_server: bool,
    /// Set where a step drives a container runtime.
    #[serde(default)]
    needs_container: bool,
    /// Compare only the lines matching this prefix. Container output carries
    /// generated names, ports and paths that differ between the two runs by
    /// design, so a few cases check the shape rather than the whole text.
    #[serde(default)]
    only_lines_starting: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Cases {
    case: Vec<Case>,
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn binary() -> PathBuf {
    // The integration test binary sits one level below the built `sqlcmd`.
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("sqlcmd{}", std::env::consts::EXE_SUFFIX))
}

/// Runs one case's steps in order and returns the last step's output.
fn capture(exe: &Path, case: &Case, config: &Path, server: Option<&Server>) -> Run {
    let mut last = Run {
        stdout: String::new(),
        stderr: String::new(),
        code: 0,
    };
    // A `needs_server` case is written against placeholders so the same case
    // works wherever the test server happens to live.
    let password = server.map_or(TEST_PASSWORD.to_string(), |s| s.password.clone());
    for step in &case.steps {
        let step: Vec<String> = step
            .iter()
            .map(|arg| match server {
                Some(s) => arg
                    .replace("{address}", &s.address)
                    .replace("{port}", &s.port)
                    .replace("{user}", &s.user),
                None => arg.clone(),
            })
            .collect();
        let output = Command::new(exe)
            .args(&step)
            .arg("--sqlconfig")
            .arg(config)
            .env("SQLCMD_PASSWORD", &password)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("cannot run {}: {e}", exe.display()));
        last = Run {
            stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
            code: output.status.code().unwrap_or(-1),
        };
    }
    last
}

/// The reference prints the default config path in its usage text, which
/// differs per machine and per `--sqlconfig`.
fn normalize(text: &str, config: &Path) -> String {
    text.replace(&config.display().to_string(), "<config>")
        .trim_end()
        .to_string()
}

/// Splits into lines, keeping only those a case cares about and sorting them
/// only where the order is not the reference's to promise.
fn comparable(text: &str, case: &Case) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| match &case.only_lines_starting {
            Some(prefix) => line.starts_with(prefix.as_str()),
            None => true,
        })
        .map(str::to_string)
        .collect();
    if case.unordered {
        lines.sort();
    }
    lines
}

/// Removes the containers a case created, whichever binary created them.
///
/// A case that fails part-way would otherwise leave a SQL Server running and
/// a port taken, and the next run would collide with it.
fn remove_containers(config: &Path) {
    let Ok(text) = std::fs::read_to_string(config) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(id) = line.strip_prefix("id:") {
            let id = id.trim().trim_matches('"');
            if id.is_empty() {
                continue;
            }
            let _ = Command::new("docker")
                .args(["rm", "--force", id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[test]
fn modern_cli_matches_go_sqlcmd() {
    let Some(reference) = std::env::var_os(GO_ENV).map(PathBuf::from) else {
        eprintln!("{GO_ENV} not set; skipping");
        return;
    };
    let mine = binary();
    let mut cases: Vec<Case> = Vec::new();
    for text in [
        include_str!("modern/cases/config.toml"),
        include_str!("modern/cases/server.toml"),
    ] {
        let parsed: Cases = toml::from_str(text).expect("case file parses");
        cases.extend(parsed.case);
    }

    let root = tempfile::tempdir().expect("temp dir");
    let server = server();
    let has_server = server.is_some();
    let containers = std::env::var(CONTAINERS_ENV).is_ok_and(|v| v == "1");

    let mut passed = 0;
    let mut skipped = 0;
    let mut failures = Vec::new();

    for case in &cases {
        if case.needs_server && !has_server {
            skipped += 1;
            eprintln!("skipped {}: {CONNECT_ENV} is not set", case.name);
            continue;
        }
        if case.needs_container && !containers {
            skipped += 1;
            eprintln!("skipped {}: set {CONTAINERS_ENV}=1 to run it", case.name);
            continue;
        }

        let slug: String = case
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let theirs_config = root.path().join(format!("{slug}-go.yaml"));
        let ours_config = root.path().join(format!("{slug}-rs.yaml"));

        // Only a `needs_server` case is written against the placeholders.
        let details = if case.needs_server {
            server.as_ref()
        } else {
            None
        };
        let theirs = capture(&reference, case, &theirs_config, details);
        let ours = capture(&mine, case, &ours_config, details);

        if case.needs_container {
            remove_containers(&theirs_config);
            remove_containers(&ours_config);
        }

        // go-sqlcmd writes its messages to stdout and stderr inconsistently
        // between commands, so the two are compared as one stream.
        let theirs_text = normalize(
            &format!("{}{}", theirs.stdout, theirs.stderr),
            &theirs_config,
        );
        let ours_text = normalize(&format!("{}{}", ours.stdout, ours.stderr), &ours_config);

        let succeeded = comparable(&theirs_text, case) == comparable(&ours_text, case)
            && (theirs.code == 0) == (ours.code == 0);
        if succeeded {
            passed += 1;
        } else {
            failures.push(format!(
                "### {}\n  reference (exit {}): {:?}\n  rust      (exit {}): {:?}",
                case.name, theirs.code, theirs_text, ours.code, ours_text
            ));
        }
    }

    println!(
        "modern: {passed} passed, {} failed, {skipped} skipped",
        failures.len()
    );
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

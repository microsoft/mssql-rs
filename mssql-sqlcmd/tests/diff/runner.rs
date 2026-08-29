// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runs one scenario against both binaries and reports where they disagree.
//!
//! Shared by the ODBC and go-sqlcmd harnesses, each of which uses a different
//! subset of what follows.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Path to the reference ODBC `sqlcmd`. Without it there is nothing to compare
/// against and every case is skipped.
pub const REF_ENV: &str = "SQLCMD_DIFF_REF";

/// Server to point connecting cases at. Cases that need one are skipped when
/// this is unset, so the suite stays useful on a machine with no SQL Server.
pub const SERVER_ENV: &str = "SQLCMD_DIFF_SERVER";

/// Arguments prepended to every connecting case, whitespace-separated.
///
/// On Windows the default local instance and integrated auth need no arguments
/// at all, so this is unset. Linux has neither, and needs something like
/// `-S localhost,1435 -U sa -P <password>`.
pub const CONNECT_ENV: &str = "SQLCMD_DIFF_CONNECT";

/// The connection prefix, or `fallback` when the variable is unset.
pub fn connect_args(fallback: &[&str]) -> Vec<String> {
    match std::env::var(CONNECT_ENV) {
        Ok(text) if !text.trim().is_empty() => {
            text.split_whitespace().map(str::to_string).collect()
        }
        _ => fallback.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// Prepends the connection prefix to every case that reaches a server.
pub fn apply_connect_prefix(cases: &mut [Case], fallback: &[&str]) {
    let prefix = connect_args(fallback);
    if prefix.is_empty() {
        return;
    }
    for case in cases.iter_mut().filter(|c| c.connect) {
        // First, so a case can still override any of it.
        let mut args = prefix.clone();
        args.append(&mut case.args);
        case.args = args;
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct Case {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Arguments given to our build only. `--compat go` uses this so the same
    /// case can be pointed at either reference.
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub stdin: Option<String>,
    /// Files to materialize in the working directory before running.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Whether the case reaches a server.
    #[serde(default)]
    pub connect: bool,
    /// Set to record a known difference without failing the suite.
    #[serde(default)]
    pub skip_reason: Option<String>,
    /// Set where the reference itself behaves differently on Unix, so the case
    /// can only hold on Windows. The reason is required, so a divergence has to
    /// be explained rather than waved away.
    #[serde(default)]
    pub unix_skip_reason: Option<String>,
}

#[derive(Debug)]
pub enum Outcome {
    Passed,
    Skipped(String),
    Failed(String),
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn capture(exe: &Path, case: &Case, dir: &Path, extra: &[String]) -> std::io::Result<Run> {
    let mut cmd = Command::new(exe);
    cmd.args(&case.args)
        .args(extra)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // A stale SQLCMD* variable in the developer's shell would silently change
    // the reference's behaviour and not ours, so start from a clean slate.
    for (key, _) in std::env::vars() {
        if key.starts_with("SQLCMD") {
            cmd.env_remove(key);
        }
    }

    let mut child = cmd.spawn()?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin was piped");
        if let Some(text) = &case.stdin {
            stdin.write_all(text.as_bytes())?;
        }
    }
    let out = child.wait_with_output()?;

    Ok(Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    })
}

/// Erase the parts that legitimately differ between the two builds.
fn normalize(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.starts_with("Version ") {
                return "Version <VERSION>".to_string();
            }
            // `Msg ..., Server <host>, Line ...` names whichever machine ran the
            // test, which is not something either build chooses.
            if line.starts_with("Msg ") {
                return mask_server(line);
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn mask_server(line: &str) -> String {
    let Some(start) = line.find(", Server ") else {
        return line.to_string();
    };
    let after = start + ", Server ".len();
    let end = line[after..]
        .find(',')
        .map(|offset| after + offset)
        .unwrap_or(line.len());
    format!("{}, Server <SERVER>{}", &line[..start], &line[end..])
}

fn diff_field(label: &str, reference: &str, actual: &str, into: &mut Vec<String>) {
    let (reference, actual) = (normalize(reference), normalize(actual));
    if reference != actual {
        into.push(format!(
            "{label}:\n  reference: {reference:?}\n  rust:      {actual:?}"
        ));
    }
}

pub fn run_case(case: &Case, reference: &Path, rust: &Path) -> Outcome {
    if let Some(reason) = &case.skip_reason {
        return Outcome::Skipped(reason.clone());
    }
    if !cfg!(windows)
        && let Some(reason) = &case.unix_skip_reason
    {
        return Outcome::Skipped(format!("on Unix: {reason}"));
    }
    if case.connect && std::env::var_os(SERVER_ENV).is_none() {
        return Outcome::Skipped(format!("{SERVER_ENV} is not set"));
    }

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return Outcome::Failed(format!("could not create a working directory: {e}")),
    };
    for (name, contents) in &case.files {
        if let Err(e) = std::fs::write(dir.path().join(name), contents) {
            return Outcome::Failed(format!("could not write {name}: {e}"));
        }
    }

    let expected = match capture(reference, case, dir.path(), &[]) {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(format!("could not run the reference: {e}")),
    };
    let actual = match capture(rust, case, dir.path(), &case.extra_args) {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(format!("could not run the rust build: {e}")),
    };

    let mut problems = Vec::new();
    diff_field("stdout", &expected.stdout, &actual.stdout, &mut problems);
    diff_field("stderr", &expected.stderr, &actual.stderr, &mut problems);
    if expected.code != actual.code {
        problems.push(format!(
            "exit code:\n  reference: {}\n  rust:      {}",
            expected.code, actual.code
        ));
    }

    if problems.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(problems.join("\n"))
    }
}

#[derive(Debug, serde::Deserialize)]
struct CaseFile {
    case: Vec<Case>,
}

pub fn load_cases() -> Vec<Case> {
    load_cases_from("tests/diff/cases")
}

/// Reads every `.toml` case file in `relative`, in name order.
pub fn load_cases_from(relative: &str) -> Vec<Case> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", dir.display()))
        .map(|e| e.expect("could not read a case entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();

    paths
        .iter()
        .flat_map(|p| {
            let text = std::fs::read_to_string(p)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", p.display()));
            let parsed: CaseFile = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("could not parse {}: {e}", p.display()));
            parsed.case
        })
        .collect()
}

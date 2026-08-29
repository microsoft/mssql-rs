// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Differential tests against the real `go-sqlcmd` binary.
//!
//! `--compat go` exists to reproduce go-sqlcmd where it and ODBC sqlcmd
//! disagree. This suite is what makes that claim checkable: every scenario runs
//! against go-sqlcmd and against our build with `--compat go`, and their
//! stdout, stderr and exit code must match.
//!
//! Set `SQLCMD_DIFF_GO` to the go-sqlcmd binary to enable the suite. It builds
//! from `go build -o <path> ./cmd/modern` in a go-sqlcmd checkout — note
//! `cmd/modern`, not `cmd/sqlcmd`, which is a library package and produces a
//! `.a` archive rather than an executable.
//!
//! Every case connects, so `SQLCMD_DIFF_SERVER` must also be set.

#[path = "diff/runner.rs"]
mod runner;

use std::path::PathBuf;

use runner::{Outcome, SERVER_ENV};

/// Path to the go-sqlcmd binary.
const GO_ENV: &str = "SQLCMD_DIFF_GO";

/// go-sqlcmd has no implicit default server, so every case names one.
const CONNECT: &[&str] = &["-S", "localhost", "-E", "-C"];

#[test]
fn compat_go_matches_go_sqlcmd() {
    let Some(go) = std::env::var_os(GO_ENV).map(PathBuf::from) else {
        eprintln!("skipping: set {GO_ENV} to the go-sqlcmd binary to run these tests");
        return;
    };
    assert!(
        go.is_file(),
        "{GO_ENV} points at {}, which is not a file",
        go.display()
    );
    if std::env::var_os(SERVER_ENV).is_none() {
        eprintln!("skipping: set {SERVER_ENV}; every go-sqlcmd case connects");
        return;
    }

    let rust = PathBuf::from(env!("CARGO_BIN_EXE_sqlcmd"));
    let cases = load_cases();
    assert!(!cases.is_empty(), "no go-sqlcmd cases were found");

    let (mut passed, mut skipped) = (0, 0);
    let mut failures = Vec::new();

    for case in &cases {
        match runner::run_case(case, &go, &rust) {
            Outcome::Passed => passed += 1,
            Outcome::Skipped(reason) => {
                skipped += 1;
                eprintln!("skipped {}: {reason}", case.name);
            }
            Outcome::Failed(detail) => failures.push(format!("\n{}\n{detail}", case.name)),
        }
    }

    eprintln!(
        "go-sqlcmd: {passed} passed, {} failed, {skipped} skipped",
        failures.len()
    );
    assert!(failures.is_empty(), "\n\n{}\n", failures.join("\n\n"));
}

/// Builds the case list, giving both binaries the same connection arguments and
/// adding `--compat go` to ours alone.
fn load_cases() -> Vec<runner::Case> {
    let mut cases = runner::load_cases_from("tests/go/cases");
    for case in &mut cases {
        case.extra_args = vec!["--compat".to_string(), "go".to_string()];
        case.connect = true;
    }
    runner::apply_connect_prefix(&mut cases, CONNECT);
    cases
}

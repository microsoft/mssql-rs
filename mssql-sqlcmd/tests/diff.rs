// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Differential tests against the shipped ODBC `sqlcmd`.
//!
//! The reference binary is the specification: for every scenario under
//! `tests/diff/cases` both tools are run with the same arguments, stdin and
//! working directory, and their stdout, stderr and exit code must match.
//!
//! Set `SQLCMD_DIFF_REF` to the reference binary to enable the suite, and
//! `SQLCMD_DIFF_SERVER` to additionally enable the cases that connect.

#[path = "diff/runner.rs"]
mod runner;

use std::path::PathBuf;

use runner::{Outcome, REF_ENV};

#[test]
fn matches_the_reference_implementation() {
    let Some(reference) = std::env::var_os(REF_ENV).map(PathBuf::from) else {
        eprintln!("skipping: set {REF_ENV} to the ODBC sqlcmd binary to run these tests");
        return;
    };
    assert!(
        reference.is_file(),
        "{REF_ENV} points at {}, which is not a file",
        reference.display()
    );

    let rust = PathBuf::from(env!("CARGO_BIN_EXE_sqlcmd"));
    let mut cases = runner::load_cases();
    // Windows reaches the default local instance with no arguments at all;
    // Linux has neither a default instance nor integrated auth, so the prefix
    // comes from the environment there.
    runner::apply_connect_prefix(&mut cases, &[]);
    assert!(!cases.is_empty(), "no differential cases were found");

    let (mut passed, mut skipped) = (0, 0);
    let mut failures = Vec::new();

    for case in &cases {
        match runner::run_case(case, &reference, &rust) {
            Outcome::Passed => passed += 1,
            Outcome::Skipped(reason) => {
                skipped += 1;
                eprintln!("skipped {}: {reason}", case.name);
            }
            Outcome::Failed(detail) => failures.push(format!("{}\n{detail}", case.name)),
        }
    }

    eprintln!(
        "differential: {passed} passed, {} failed, {skipped} skipped",
        failures.len()
    );
    assert!(failures.is_empty(), "\n\n{}\n", failures.join("\n\n"));
}

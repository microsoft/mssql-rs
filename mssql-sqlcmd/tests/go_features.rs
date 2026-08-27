// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Features go-sqlcmd adds that ODBC sqlcmd has no equivalent for.
//!
//! These cannot be checked differentially — the reference rejects the flags
//! outright — so the expected output is written down here instead. Cases that
//! need a server are gated on `SQLCMD_DIFF_SERVER` the same way the
//! differential suite is.

use std::process::{Command, Stdio};

const SERVER_ENV: &str = "SQLCMD_DIFF_SERVER";

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn sqlcmd(args: &[&str]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sqlcmd"));
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars() {
        if key.starts_with("SQLCMD") {
            cmd.env_remove(key);
        }
    }
    let out = cmd.output().expect("the binary should run");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        stderr: String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n"),
        code: out.status.code().unwrap_or(-1),
    }
}

fn have_server() -> bool {
    std::env::var_os(SERVER_ENV).is_some()
}

#[test]
fn version_prints_the_banner_and_stops() {
    let run = sqlcmd(&["--version"]);
    assert_eq!(run.code, 0);
    assert!(
        run.stdout
            .starts_with("Microsoft (R) SQL Server Command Line Tool\n")
    );
    assert!(run.stdout.contains("Version "));
    // The banner alone, without the usage block `-?` adds.
    assert!(!run.stdout.contains("usage: Sqlcmd"));
}

#[test]
fn the_two_layouts_are_mutually_exclusive() {
    let run = sqlcmd(&["--vertical", "--ascii"]);
    assert_eq!(run.code, 1);
    assert_eq!(
        run.stderr,
        "Sqlcmd: The --vertical and the --ascii options are mutually exclusive.\n"
    );
}

#[test]
fn an_inferred_and_a_named_auth_method_are_mutually_exclusive() {
    let run = sqlcmd(&["-G", "--authentication-method", "ActiveDirectoryDefault"]);
    assert_eq!(run.code, 1);
    assert_eq!(
        run.stderr,
        "Sqlcmd: The -G and the --authentication-method options are mutually exclusive.\n"
    );
}

#[test]
fn an_unknown_auth_method_is_refused_rather_than_ignored() {
    let run = sqlcmd(&["--authentication-method", "Nonsense"]);
    assert_eq!(run.code, 1);
    assert_eq!(
        run.stderr,
        "Sqlcmd: 'Nonsense': Unsupported authentication method.\n"
    );
}

#[test]
fn server_name_is_refused_rather_than_silently_ignored() {
    let run = sqlcmd(&["--server-name", "other"]);
    assert_eq!(run.code, 1);
    assert_eq!(
        run.stderr,
        "Sqlcmd: '--server-name': Not supported by this build.\n"
    );
}

#[test]
fn vertical_prints_one_field_per_line() {
    if !have_server() {
        return;
    }
    let run = sqlcmd(&["-C", "--vertical", "-Q", "SELECT 1 AS id, 'Alice' AS name"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "id   1\nname Alice\n\n\n(1 rows affected)\n");
}

#[test]
fn vertical_field_names_are_padded_to_the_longest() {
    if !have_server() {
        return;
    }
    let run = sqlcmd(&["-C", "--vertical", "-Q", "SELECT 1 AS a, 2 AS longer"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "a      1\nlonger 2\n\n\n(1 rows affected)\n");
}

#[test]
fn ascii_draws_a_bordered_table() {
    if !have_server() {
        return;
    }
    let run = sqlcmd(&[
        "-C",
        "--ascii",
        "-Q",
        "SELECT 1 AS id, 'Alice' AS name UNION ALL SELECT 22, 'Bob'",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(
        run.stdout,
        "+----+-------+\n\
         | id | name  |\n\
         +----+-------+\n\
         |  1 | Alice |\n\
         | 22 | Bob   |\n\
         +----+-------+\n\
         (2 rows affected)\n"
    );
}

#[test]
fn ascii_uses_the_column_separator_for_its_borders() {
    if !have_server() {
        return;
    }
    let run = sqlcmd(&["-C", "--ascii", "-s", "#", "-Q", "SELECT 1 AS a"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert_eq!(
        run.stdout,
        "+---+\n# a #\n+---+\n# 1 #\n+---+\n(1 rows affected)\n"
    );
}

#[test]
fn the_format_option_selects_a_layout_by_name() {
    if !have_server() {
        return;
    }
    let by_name = sqlcmd(&["-C", "--format", "vert", "-Q", "SELECT 1 AS a"]);
    let by_flag = sqlcmd(&["-C", "--vertical", "-Q", "SELECT 1 AS a"]);
    assert_eq!(by_name.stdout, by_flag.stdout);
}

#[test]
fn statistics_follow_each_batch() {
    if !have_server() {
        return;
    }
    let run = sqlcmd(&["-C", "-p", "-Q", "SELECT 1 AS a"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("Network packet size (bytes): 4096"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("1 xact[s]:"), "{}", run.stdout);
    assert!(
        run.stdout.contains("Clock Time (ms.): total"),
        "{}",
        run.stdout
    );
}

#[test]
fn the_colon_statistics_form_is_a_single_line() {
    if !have_server() {
        return;
    }
    let run = sqlcmd(&["-C", "-p1", "-Q", "SELECT 1 AS a"]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let stats = run
        .stdout
        .lines()
        .find(|l| l.starts_with("4096:"))
        .unwrap_or_else(|| panic!("no statistics line in {}", run.stdout));
    // packet:xacts:total:avg:persec, and the reference leaves a trailing space.
    assert_eq!(stats.split(':').count(), 5, "{stats}");
    assert!(stats.ends_with(' '), "{stats:?}");
}

#[test]
fn a_trace_file_records_each_batch() {
    if !have_server() {
        return;
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let trace = dir.path().join("trace.txt");
    let run = sqlcmd(&[
        "-C",
        "--trace-file",
        trace.to_str().unwrap(),
        "-Q",
        "SELECT 1 AS a",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);

    let recorded = std::fs::read_to_string(&trace).expect("the trace file should exist");
    assert!(recorded.contains("batch:"), "{recorded}");
    // Diagnostics must not leak into the results the caller parses.
    assert!(!run.stdout.contains("batch:"), "{}", run.stdout);
}

#[test]
fn a_trace_file_that_cannot_be_opened_is_reported() {
    let run = sqlcmd(&[
        "--trace-file",
        "no/such/directory/trace.txt",
        "-Q",
        "SELECT 1",
    ]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("Invalid filename"), "{}", run.stderr);
}

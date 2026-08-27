// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Colour behaviour that can be checked without a terminal.
//!
//! The end-to-end colour comparison runs on Linux, where `script(1)` can
//! allocate a pseudo-terminal to make the tools believe they are on a console.
//! The harness has no Windows equivalent, so these cover the parts that need no
//! PTY:
//!
//! - the gate itself, which is what a script capturing our output relies on;
//! - `:list color`, which names the schemes.
//!
//! The escape sequences themselves are asserted in `fmt::color`'s unit tests,
//! against bytes captured from the reference through a PTY. Together these run
//! on every platform, so a Windows-only regression in the gate is caught here
//! rather than going unnoticed until someone tries it by hand.

use std::process::{Command, Stdio};

/// Connect arguments for the cases that reach a server, e.g.
/// `-S localhost,1435 -U sa -P … -C`. Integrated auth is not assumed: on Linux
/// `-E` needs a Kerberos ticket and would fail before anything was rendered.
const CONNECT_ENV: &str = "SQLCMD_DIFF_CONNECT";

fn connect_args() -> Option<Vec<String>> {
    let value = std::env::var(CONNECT_ENV).ok()?;
    let args: Vec<String> = value.split_whitespace().map(str::to_string).collect();
    (!args.is_empty()).then_some(args)
}

/// Runs sqlcmd with a colour scheme set, capturing both streams. `Command`
/// gives the child pipes, so this is never a terminal — which is the point.
fn captured(scheme: &str, args: &[&str]) -> (Vec<u8>, Vec<u8>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sqlcmd"));
    for (key, _) in std::env::vars() {
        if key.starts_with("SQLCMD") {
            cmd.env_remove(key);
        }
    }
    let out = cmd
        .args(args)
        .env("SQLCMDCOLORSCHEME", scheme)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("the binary should run");
    (out.stdout, out.stderr)
}

/// Runs a query against the configured server, or returns `None` when there is
/// no server to run it against.
fn captured_query(scheme: &str, sql: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut args = connect_args()?;
    args.push("-Q".to_string());
    args.push(sql.to_string());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    Some(captured(scheme, &borrowed))
}

#[test]
fn a_captured_stream_carries_no_escape_sequences() {
    // This is the case that would break a pipeline: a script reading our output
    // must not have to strip escapes. `nosuchscheme` is included because an
    // unknown name still resolves to chroma's fallback style, so it colours.
    for scheme in ["monokai", "github", "nosuchscheme"] {
        let Some((stdout, _)) = captured_query(scheme, "SELECT 1 AS a") else {
            return;
        };
        assert!(
            !stdout.contains(&0x1B),
            "scheme {scheme:?} leaked an escape sequence into captured output"
        );
    }
}

#[test]
fn a_captured_error_is_left_plain_too() {
    // Errors are drawn from a different face and may travel on a different
    // stream, so the gate has to hold for both.
    let Some((stdout, stderr)) = captured_query("monokai", "SELECT * FROM nope") else {
        return;
    };
    assert!(!stdout.contains(&0x1B), "stdout leaked an escape");
    assert!(!stderr.contains(&0x1B), "stderr leaked an escape");
}

#[test]
fn list_color_names_the_schemes_without_colouring_them() {
    // `:list color` answers locally, but sqlcmd connects before it reads any
    // input, so this still needs a reachable server.
    let Some(mut args) = connect_args() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("sqlcmd-color-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let script = dir.join("in.sql");
    std::fs::write(&script, ":list color\n").expect("write script");

    args.push("-i".to_string());
    args.push(script.to_string_lossy().into_owned());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let (stdout, _) = captured("monokai", &borrowed);
    let text = String::from_utf8_lossy(&stdout);

    assert!(!stdout.contains(&0x1B), "`:list color` leaked an escape");
    // A few well-known names, including the fallback an unknown name resolves to.
    for name in ["monokai", "swapoff", "github"] {
        assert!(text.contains(name), "`:list color` omitted {name:?}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

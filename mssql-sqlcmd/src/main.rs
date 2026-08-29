// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `sqlcmd` — a Rust implementation of the SQL Server command line tool.
//!
//! Command-line compatibility with the ODBC `sqlcmd` is a hard requirement, so
//! option grammar, diagnostics, output layout and exit codes are verified
//! against the shipped binary by the differential tests in `tests/diff.rs`.
//!
//! The tool itself lives in the library, so the same code can be linked into
//! the native binary. This is only the standalone entry point.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // `:exit(query)` can ask for a value outside a byte, and the reference
    // passes it through unchanged, so bypass `ExitCode`.
    std::process::exit(mssql_sqlcmd::run(argv));
}

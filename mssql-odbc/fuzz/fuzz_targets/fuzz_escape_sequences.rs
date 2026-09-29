// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz ODBC escape-sequence translation — a hand-written lexer over
//! application-supplied SQL that has to survive unbalanced braces, unterminated
//! literals and comments, and arbitrary nesting. Looks for panics, hangs, and
//! violations of the phase-separation invariants.
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_escape_sequences

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_escape_sequences;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    let input = String::from_utf8_lossy(data);
    fuzz_escape_sequences(&input);
});

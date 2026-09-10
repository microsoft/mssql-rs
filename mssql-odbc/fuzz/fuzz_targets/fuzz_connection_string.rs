// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz the ODBC `SQLDriverConnect` connection-string parser — a single-pass
//! state machine over caller-controlled text with msodbcsql brace/`;`/`=`
//! quirks. Looks for panics, hangs, and non-terminating parses.
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_connection_string

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_connection_string;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    let input = String::from_utf8_lossy(data);
    fuzz_connection_string(&input);
});

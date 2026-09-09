// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz character→numeric parsing (`parse_numeric_text`) plus the exact-value
//! model's downstream narrowing to fixed-width integers. Looks for panics and
//! arithmetic overflow on decimal, exponent, and wide-decimal literals.
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_numeric_parse

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_numeric_text;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1024 {
        return;
    }
    let input = String::from_utf8_lossy(data);
    fuzz_numeric_text(&input);
});

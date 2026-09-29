// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz the character date / time / datetime literal parsers in the fetch
//! conversion path. Looks for panics and bad byte-slicing on multi-byte input
//! (the datetime parser matches a trailing UTC offset over raw bytes).
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_datetime_parse

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_datetime_literal;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1024 {
        return;
    }
    let input = String::from_utf8_lossy(data);
    fuzz_datetime_literal(&input);
});

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz the result-fetch conversion matrix: a decoded `ColumnValues` written
//! into a fixed-width `SQL_C_*` application buffer, the path `SQLGetData` runs
//! per column. The wrapper mirrors `get_data::convert_typed_c`'s routing over a
//! valid 64-byte target, so this hunts for arithmetic overflow panics in the
//! numeric narrowing and the calendar/clock extraction — not synthetic UB.
//!
//! First byte picks the C target type, the next the source column variant, the
//! rest fills that variant's fields (little-endian).
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_fetch_convert

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_fetch_convert;

fuzz_target!(|data: &[u8]| {
    if data.len() > 8192 {
        return;
    }
    fuzz_fetch_convert(data);
});

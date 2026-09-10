// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz the data-at-execution transcoding path (`transcode_dae_bytes`) that a
//! buffered `SQLPutData` value passes through before it reaches the wire. The
//! function re-encodes caller bytes across the C-type / SQL-type / collation
//! matrix (UTF-8 <-> UTF-16LE, narrow code-page encode), so this hunts for
//! panics on malformed or truncated multi-byte / surrogate input.
//!
//! First byte selects the C-type / SQL-type / collation pairing; the rest is
//! the streamed parameter value.
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_transcode

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_transcode_dae_bytes;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > 8192 {
        return;
    }
    let mode = data[0];
    fuzz_transcode_dae_bytes(&data[1..], mode);
});

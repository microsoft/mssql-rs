// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz the unsafe UTF-16 buffer readers (`read_utf16`, `read_utf16_long`,
//! `read_utf16_attr`) that decode caller `SQLWCHAR` buffers at the FFI boundary.
//! The wrapper backs the pointer with a real slice, so this hunts for
//! out-of-bounds reads and panics in the length/NUL-terminator handling — not
//! for use-after-free of a synthetic pointer.
//!
//! First byte selects the reader/length mode; the rest is decoded as
//! little-endian `SQLWCHAR` units.
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_read_utf16

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_read_utf16;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > 8192 {
        return;
    }
    let mode = data[0];
    let units: Vec<u16> = data[1..]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    fuzz_read_utf16(&units, mode);
});

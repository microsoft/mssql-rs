// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz the bind-parameter read + convert path (`bound_param_to_rpc`) that
//! `SQLExecute` runs over an application's value and length/indicator buffers.
//! The wrapper backs the value buffer with the input plus trailing zero padding
//! wide enough for the widest fixed-width C target, and confines the indicator
//! to in-bounds values, so this hunts for real conversion defects (bad length
//! arithmetic, transcode panics) rather than harness-induced out-of-bounds
//! reads.
//!
//! First bytes pick the C type, SQL type, indicator mode, column size, and
//! scale; the rest is the parameter value buffer.
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_bound_param

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_bound_param;

fuzz_target!(|data: &[u8]| {
    if data.len() > 8192 {
        return;
    }
    fuzz_bound_param(data);
});

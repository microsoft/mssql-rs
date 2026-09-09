// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz the driver's `extern "C"` result path end to end. A fresh ENV+DBC is
//! built through the real handle allocators, an in-memory `TdsClient` reading
//! the fuzz input is installed as the connection, then `SQLExecDirectW` →
//! `SQLFetch` → `SQLGetData` run over that fuzzer-controlled TDS response — the
//! way a hostile or corrupt server would feed the COLMETADATA → ROW → convert
//! pipeline. Unlike the leaf converter targets, this crosses the C ABI the
//! shipped driver exposes and drives the statement/cursor state machine.
//!
//! First byte picks the `SQLGetData` target C type; the rest is the server's
//! response stream.
//!
//! Run: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_ffi_execute

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssqlodbc::fuzz_support::fuzz_ffi_execute;

fuzz_target!(|data: &[u8]| {
    fuzz_ffi_execute(data);
});

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz target for the DATACLASSIFICATION (0xA3) token payload parser.
//!
//! The token-stream fuzzer cannot reach this parser's body because it parses
//! with a default context, and 0xA3 requires an explicitly negotiated version.
//! This target drives the parser directly with both supported versions.
//!
//! Input layout:
//! - First byte: version selector (even = v1, odd = v2)
//! - Remaining bytes: token payload following the 0xA3 token byte
//!
//! Run with: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_data_classification

#![no_main]

use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use mssql_tds::fuzz_support::fuzz_data_classification;
use tokio::runtime::{Builder, Runtime};

// Reused across iterations so runtime construction does not dominate the
// per-input cost. Each call parses from a fresh reader and spawns no tasks,
// so no state carries between inputs.
static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_current_thread()
        .build()
        .expect("failed to build fuzz runtime")
});

fuzz_target!(|data: &[u8]| {
    let Some((&version_selector, payload)) = data.split_first() else {
        return;
    };

    if payload.len() > 4096 {
        return;
    }

    RUNTIME.block_on(async {
        let _ = fuzz_data_classification(payload, version_selector).await;
    });
});

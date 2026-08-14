// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process-wide Tokio runtime backing the async surface (`PyAsyncConnection`
//! and the `pyo3_async_runtimes::tokio` bridge).
//!
//! Scope: this runtime serves only the async API. The synchronous
//! `PyCoreConnection` still owns its own per-connection `tokio::runtime::Runtime`
//! for its `block_on` calls and does not touch this shared runtime today.
//!
//! Rationale:
//! * A single multi-threaded runtime is created lazily on first async use and
//!   reused for every `PyAsyncConnection` and every Python awaitable returned
//!   via `future_into_py`, so async connection count does not multiply
//!   worker-thread pools.
//! * The runtime is registered with `pyo3_async_runtimes::tokio::init` so
//!   `future_into_py` and `get_runtime()` both resolve to the same executor,
//!   event loop, and I/O driver.
//! * Worker-thread count follows Tokio's default (one per logical CPU).

use std::sync::Once;

use tokio::runtime::Builder;

const THREAD_NAME: &str = "mssql-py-core";

static INIT: Once = Once::new();

/// Initialize the shared runtime. Idempotent; safe to call from `#[pymodule]`.
pub(crate) fn init() {
    INIT.call_once(|| {
        let mut builder = Builder::new_multi_thread();
        builder.enable_all().thread_name(THREAD_NAME);
        pyo3_async_runtimes::tokio::init(builder);
    });
}

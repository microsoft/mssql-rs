// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process-wide Tokio runtime shared by every `PyCoreConnection` and by the
//! Python `asyncio` bridge (`pyo3_async_runtimes::tokio`).
//!
//! Rationale:
//! * A single multi-threaded runtime is created lazily on first use and reused
//!   for every connection, cursor, and awaitable returned to Python. This
//!   avoids the per-connection worker-thread explosion of the previous model
//!   where each `PyCoreConnection` owned its own [`tokio::runtime::Runtime`].
//! * The runtime is handed to `pyo3_async_runtimes::tokio::init` so that both
//!   synchronous (`Handle::block_on`) and asynchronous (`future_into_py`) paths
//!   share the exact same executor, event loop, and I/O driver.
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

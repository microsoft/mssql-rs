// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process-wide Tokio runtime shared by all async PyO3 bindings.
//!
//! `pyo3-async-runtimes` uses this runtime to drive Rust futures returned
//! from `#[pymethods]` via `future_into_py`. All async classes
//! (`PyCoreAsyncConnection`, `PyCoreAsyncCursor`) resolve their work here.
//!
//! The existing synchronous `PyCoreConnection` still holds its own
//! per-connection `Runtime` for zero-risk compatibility with the bulk copy
//! path; migrating it is tracked as a follow-up.

use once_cell::sync::OnceCell;
use tokio::runtime::{Builder, Runtime};

static RUNTIME: OnceCell<Runtime> = OnceCell::new();

/// Returns the process-wide shared Tokio runtime, building it on first use.
///
/// Uses a multi-thread scheduler so two `PyCoreAsyncConnection`s can execute
/// TDS work truly in parallel.
pub(crate) fn shared_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .thread_name("mssql-py-core")
            .build()
            .expect("failed to build shared Tokio runtime for mssql_py_core")
    })
}

/// Registers the shared runtime with `pyo3-async-runtimes` so that
/// `future_into_py` uses it for every future the async classes hand back
/// to Python.
///
/// Idempotent: safe to call more than once (subsequent calls are no-ops).
pub(crate) fn install() {
    // `init_with_runtime` takes `&'static Runtime`; `shared_runtime()`
    // already returns exactly that. It errors only when a runtime is
    // already installed, which is fine — the first caller wins.
    let _ = pyo3_async_runtimes::tokio::init_with_runtime(shared_runtime());
}

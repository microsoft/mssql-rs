// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous cursor API for the Core TDS backend.
//!
//! # ⚠️ Preview API — unstable
//!
//! The types and methods in this module are gated behind the `async-preview`
//! Cargo feature and are **not** part of the stable `mssql-py-core` surface.
//! Signatures, error behavior, and internal semantics may change without
//! notice in any release.
//!
//! Sibling of `cursor.rs` (the synchronous surface). A [`PyAsyncCursor`] is
//! bound to exactly one [`crate::async_connection::PyAsyncConnection`] and
//! shares that connection's `TdsClient` via an `Arc<tokio::sync::Mutex<_>>`.
//! All I/O methods will submit their futures to the shared process-wide
//! Tokio runtime and return Python awaitables through
//! `pyo3_async_runtimes::tokio::future_into_py`.
//!
//! Invariant: one connection ↔ one cursor ↔ one `TdsClient` ↔ one TDS wire
//! session. The invariant is expressed by design, not by a runtime flag:
//! creating a second cursor is not forbidden, but both would serialize on
//! the same `Mutex<TdsClient>` and share the same TDS session, so no wire
//! corruption is possible.

use std::sync::Arc;

use pyo3::prelude::*;
use tokio::sync::Mutex;

use mssql_tds::connection::tds_client::TdsClient;

/// Asynchronous Python cursor backed by the Core TDS client.
///
/// # ⚠️ Preview API — unstable
///
/// This class is part of the `async-preview` surface. The API, method
/// signatures, error behavior, and internal semantics may change without
/// notice in minor releases. Do not depend on it from production code.
///
/// Created via [`crate::async_connection::PyAsyncConnection::cursor`].
/// Instances share the parent connection's `TdsClient` — closing the
/// connection while cursors exist is legal but any in-flight I/O will fail
/// once the underlying transport is torn down.
#[pyclass]
pub struct PyAsyncCursor {
    /// Cloned from the parent `PyAsyncConnection`. The `Arc` keeps the
    /// client alive across cursor and connection lifetimes; the async mutex
    /// serializes wire access across `.await` points.
    #[allow(dead_code)] // Consumed by upcoming async execute/fetch/close APIs.
    tds_client: Arc<Mutex<TdsClient>>,
}

impl PyAsyncCursor {
    /// Construct a new cursor bound to the given TDS client.
    ///
    /// Called only from `PyAsyncConnection::cursor`.
    pub(crate) fn new(tds_client: Arc<Mutex<TdsClient>>) -> Self {
        Self { tds_client }
    }
}

#[pymethods]
impl PyAsyncCursor {
    // Async execute/fetch/close APIs land here as they are added.
}

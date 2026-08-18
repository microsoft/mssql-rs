// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous cursor API for the Core TDS backend.
//!
//! # ⚠️ Preview API — unstable
//!
//! The types and methods in this module are **not** part of the stable
//! `mssql-py-core` surface. Signatures, error behavior, and internal
//! semantics may change without notice in any release.
//!
//! Sibling of `cursor.rs` (the synchronous surface). A [`PyAsyncCursor`] is
//! bound to a single [`crate::async_connection::PyAsyncConnection`] and
//! shares that connection's `TdsClient` via an `Arc<tokio::sync::Mutex<_>>`.
//! All I/O methods will submit their futures to the shared process-wide
//! Tokio runtime and return Python awaitables through
//! `pyo3_async_runtimes::tokio::future_into_py`.
//!
//! Invariant: one `TdsClient` per connection, one TDS wire session per
//! `TdsClient`, and all access serialized through the async mutex. Creating
//! a second cursor on the same connection is allowed — both cursors share
//! the same client and serialize on the same mutex, so wire integrity is
//! preserved.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use pyo3::prelude::*;
use tokio::sync::Mutex;

use mssql_tds::connection::tds_client::TdsClient;

use crate::async_session::{AsyncConnectionState, CursorId};

/// Asynchronous Python cursor backed by the Core TDS client.
///
/// # ⚠️ Preview API — unstable
///
/// Preview surface: API, method signatures, error behavior, and internal
/// semantics may change without notice in minor releases. Do not depend on
/// it from production code.
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
    /// Shared with the parent connection for future execute-time transaction handling.
    #[allow(dead_code)]
    autocommit: Arc<AtomicBool>,
    /// Connection-wide ownership and lifecycle state shared by all cursors.
    #[allow(dead_code)]
    session_state: Arc<AsyncConnectionState>,
    /// Stable identity used to claim results and target cancellation.
    #[allow(dead_code)]
    cursor_id: CursorId,
    /// Snapshot of the parent connection's default query timeout at
    /// `cursor()` time (`0` = no timeout). Applied by the future `execute`
    /// path unless overridden per-call.
    default_query_timeout: u32,
}

impl PyAsyncCursor {
    /// Construct a new cursor bound to the given TDS client.
    ///
    /// Called only from `PyAsyncConnection::cursor`.
    pub(crate) fn new(
        tds_client: Arc<Mutex<TdsClient>>,
        autocommit: Arc<AtomicBool>,
        session_state: Arc<AsyncConnectionState>,
        cursor_id: CursorId,
        default_query_timeout: u32,
    ) -> Self {
        Self {
            tds_client,
            autocommit,
            session_state,
            cursor_id,
            default_query_timeout,
        }
    }
}

#[pymethods]
impl PyAsyncCursor {
    /// Query timeout (seconds) snapshotted from the parent connection. `0` means no timeout.
    #[getter]
    fn timeout(&self) -> u32 {
        self.default_query_timeout
    }

    // TODO(async execute transaction semantics):
    // - Hold the shared client mutex across the active-transaction check,
    //   optional begin, and statement submission.
    // - When autocommit is false, lazily begin before execution if no
    //   transaction is active.
    // - After commit or rollback, begin a fresh transaction on the next execute.
    // - When autocommit is true, submit without an explicit transaction.
    // - Add an awaitable connection mode-change API rather than a synchronous
    //   property setter: false -> true commits active work before changing
    //   mode and leaves the mode unchanged if commit fails; true -> false
    //   defers begin until the next execute.
    // - Test lazy begin, restart after commit/rollback, both mode transitions,
    //   context finalization, and cleanup-error precedence.
    //
    // TODO(async execute/fetch/cancel ownership):
    // - Execute claims ActiveOperation with this cursor_id and a fresh
    //   operation_id before touching TdsClient; reject another cursor with a
    //   clear connection-busy error.
    // - Preserve ownership while results remain and transition the phase from
    //   Executing to Fetching; release it on exhaustion, close, cancel, or error.
    // - Install a root CancelHandle in ActiveOperation and pass a child handle
    //   to TdsClient. cancel() may remove and trigger it only when both cursor
    //   and operation IDs match.
    // - Completion clears state only for its own operation_id so a stale future
    //   cannot release a newer operation.
    // - Reject new work when lifecycle is Closing, Closed, or Broken; mark the
    //   connection Broken when cancellation or protocol failure makes reuse unsafe.
    // - Add a two-cursor acceptance test with a multi-packet result: reject
    //   cursor B with a typed busy-state error while cursor A has unread rows,
    //   verify A can drain its remaining rows, then verify B can execute after
    //   A drains or closes.
}

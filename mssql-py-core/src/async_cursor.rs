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
//! retains its revocable logical parent rather than owning the physical client.
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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use mssql_tds::error::Error;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use crate::async_description::DescriptionState;
use crate::async_errors::ProgrammingError;
use crate::async_execute::{ExecuteResources, PreparedState, release_prepared_statements};
use crate::async_fetch::{BufferedResults, FetchState};
use crate::async_handle::LogicalConnection;
use crate::async_session::{
    AsyncConnectionState, ClaimError, CursorCloseClaim, CursorId, SessionOperationGuard,
};
use crate::async_tracing::{in_cursor_operation_span, record_result_set_status};

/// Converts a failed session claim into a Python error with operation-specific busy text.
fn map_claim_error_with_busy_message(error: ClaimError, busy_message: &'static str) -> PyErr {
    match error {
        ClaimError::Closing => PyRuntimeError::new_err("Connection is closing"),
        ClaimError::Closed => PyRuntimeError::new_err("Connection is closed"),
        ClaimError::Broken => PyRuntimeError::new_err("Connection is broken"),
        ClaimError::Busy => PyRuntimeError::new_err(busy_message),
        ClaimError::NoResultSet => ProgrammingError::new_err("No active result set"),
    }
}

/// Converts a failed cursor operation claim into a Python error.
pub(crate) fn map_claim_error(error: ClaimError) -> PyErr {
    map_claim_error_with_busy_message(error, "Connection is busy with another cursor operation")
}

fn clear_result_state(
    fetch_state: &FetchState,
    description_state: &DescriptionState,
    buffered_results: &BufferedResults,
) {
    buffered_results.replace(Default::default());
    description_state.replace(None);
    fetch_state.set(crate::async_fetch::FetchStatus::NoResultSet);
}

/// Python-independent resources required to drain and release a cursor.
struct CursorCleanup {
    client: Arc<LogicalConnection>,
    prepared_state: Arc<Mutex<PreparedState>>,
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    timeout: u32,
    closed: Arc<AtomicBool>,
    fetch_state: Arc<FetchState>,
    description_state: Arc<DescriptionState>,
    buffered_results: Arc<BufferedResults>,
}

impl CursorCleanup {
    fn clear_result_state(&self) {
        clear_result_state(
            &self.fetch_state,
            &self.description_state,
            &self.buffered_results,
        );
    }

    /// Borrow through the close claim; a revoked parent needs only local cursor cleanup.
    async fn run(self, claim: CursorCloseClaim) -> Result<(), Error> {
        let mut cleanup_guard =
            SessionOperationGuard::new(self.session_state.clone(), claim.operation_id);
        let (result, has_open_batch) = {
            let mut client = match self.client.lock_operation(claim.operation_id).await {
                Ok(client) => client,
                Err(ClaimError::Closing | ClaimError::Closed) => {
                    cleanup_guard.settle(false);
                    self.clear_result_state();
                    self.closed.store(true, Ordering::Release);
                    return Ok(());
                }
                Err(error) => {
                    cleanup_guard.settle(false);
                    return Err(Error::UsageError(format!(
                        "Cursor cleanup rejected: {error:?}"
                    )));
                }
            };
            let result = async {
                if claim.drain_previous {
                    client.close_query().await?;
                }
                release_prepared_statements(&mut client, &self.prepared_state, self.timeout)
                    .await?;
                Ok::<(), Error>(())
            }
            .await;
            let has_open_batch = client.has_open_batch();
            (result, has_open_batch)
        };

        cleanup_guard.settle(has_open_batch);
        // Cleanup consumes the close attempt even when draining or unprepare fails.
        self.clear_result_state();
        self.closed.store(true, Ordering::Release);
        result?;
        Ok(())
    }
}

/// Ensures abandoned finalizer tasks synchronously poison session ownership.
struct FinalizerCleanup {
    cleanup: Option<CursorCleanup>,
    completion_guard: FinalizerCompletionGuard,
}

/// Abandon session ownership if a scheduled finalizer never finishes its cleanup.
struct FinalizerCompletionGuard {
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    completed: bool,
}

impl FinalizerCleanup {
    async fn run(mut self, claim: CursorCloseClaim) {
        let cleanup = self.cleanup.take().expect("finalizer cleanup is available");
        if let Err(error) = cleanup.run(claim).await {
            record_result_set_status("error");
            tracing::warn!("PyAsyncCursor finalizer cleanup failed: {error}");
        } else {
            record_result_set_status("closed");
        }
        self.completion_guard.complete();
    }
}

impl FinalizerCompletionGuard {
    fn new(session_state: Arc<AsyncConnectionState>, cursor_id: CursorId) -> Self {
        Self {
            session_state,
            cursor_id,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for FinalizerCompletionGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.session_state.abandon_cursor(self.cursor_id);
        }
    }
}

/// Asynchronous Python cursor backed by the Core TDS client.
///
/// Create instances with `PyAsyncConnection.cursor()`. Cursors on one
/// connection share a TDS session, so only one cursor may own an active batch.
/// A row-producing execute retains ownership until the same cursor is
/// re-executed or closed; other cursors and `commit()` or `rollback()` report busy.
/// This API is a preview and may change between minor releases.
#[pyclass]
pub struct PyAsyncCursor {
    /// Keeps the logical parent alive, but cannot bypass explicit connection close.
    tds_client: Arc<LogicalConnection>,
    tracing_dispatch: Option<tracing::Dispatch>,
    prepared_state: Arc<Mutex<PreparedState>>,
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
    arraysize: isize,
    input_sizes: Option<Vec<crate::types::ParameterHint>>,
    input_sizes_generation: u64,
    cleanup_required: Arc<AtomicBool>,
    cleanup_started: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    fetch_state: Arc<FetchState>,
    description_state: Arc<DescriptionState>,
    rowcount: Arc<AtomicI64>,
    buffered_results: Arc<BufferedResults>,
}

impl PyAsyncCursor {
    /// Construct a cursor retaining its logical connection.
    ///
    /// Called only from `PyAsyncConnection::cursor`.
    pub(crate) fn new(
        tds_client: Arc<LogicalConnection>,
        tracing_dispatch: Option<tracing::Dispatch>,
        autocommit: Arc<AtomicBool>,
        session_state: Arc<AsyncConnectionState>,
        cursor_id: CursorId,
        default_query_timeout: u32,
    ) -> Self {
        Self {
            tds_client,
            tracing_dispatch,
            prepared_state: Arc::new(Mutex::new(PreparedState::default())),
            autocommit,
            session_state,
            cursor_id,
            default_query_timeout,
            arraysize: 1,
            input_sizes: None,
            input_sizes_generation: 0,
            cleanup_required: Arc::new(AtomicBool::new(false)),
            cleanup_started: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            fetch_state: Arc::new(FetchState::new()),
            description_state: Arc::new(DescriptionState::new()),
            rowcount: Arc::new(AtomicI64::new(-1)),
            buffered_results: Arc::new(BufferedResults::default()),
        }
    }

    /// Retain the logical parent for cleanup without giving the finalizer physical ownership.
    fn cleanup(&self) -> CursorCleanup {
        CursorCleanup {
            client: self.tds_client.clone(),
            prepared_state: self.prepared_state.clone(),
            session_state: self.session_state.clone(),
            cursor_id: self.cursor_id,
            timeout: self.default_query_timeout,
            closed: self.closed.clone(),
            fetch_state: self.fetch_state.clone(),
            description_state: self.description_state.clone(),
            buffered_results: self.buffered_results.clone(),
        }
    }

    /// Capture fetch state and parent lifetime before releasing the Python borrow.
    pub(crate) fn fetch_resources(&self) -> PyResult<crate::async_fetch::FetchResources> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Cursor is closed"));
        }
        Ok(crate::async_fetch::FetchResources::new(
            self.tds_client.clone(),
            self.tracing_dispatch.clone(),
            self.session_state.clone(),
            self.cursor_id,
            self.fetch_state.clone(),
            self.description_state.clone(),
            self.buffered_results.clone(),
            self.rowcount.clone(),
        ))
    }

    /// Snapshot execution inputs while retaining revocable, not raw-client, access.
    pub(crate) fn execute_resources(&self) -> PyResult<ExecuteResources> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Cursor is closed"));
        }
        Ok(ExecuteResources::new(
            self.tds_client.clone(),
            self.tracing_dispatch.clone(),
            self.prepared_state.clone(),
            self.autocommit.load(Ordering::Relaxed),
            self.session_state.clone(),
            self.cursor_id,
            self.default_query_timeout,
            self.input_sizes.clone(),
            self.input_sizes_generation,
            self.cleanup_required.clone(),
            self.closed.clone(),
            self.fetch_state.clone(),
            self.description_state.clone(),
            self.rowcount.clone(),
            self.buffered_results.clone(),
        ))
    }

    pub(crate) fn input_sizes_generation(&self) -> u64 {
        self.input_sizes_generation
    }

    pub(crate) fn replace_input_sizes(
        &mut self,
        input_sizes: Option<Vec<crate::types::ParameterHint>>,
    ) -> PyResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Cursor is closed"));
        }
        self.input_sizes = input_sizes;
        self.input_sizes_generation = self.input_sizes_generation.wrapping_add(1);
        Ok(())
    }

    pub(crate) fn clear_input_sizes(&mut self) {
        self.input_sizes = None;
    }
}

impl Drop for PyAsyncCursor {
    fn drop(&mut self) {
        if !self.cleanup_required.load(Ordering::Acquire)
            || self.closed.load(Ordering::Acquire)
            || self
                .cleanup_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }

        let cleanup = self.cleanup();
        let session_state = self.session_state.clone();
        let claim = match session_state.claim_cursor_close(self.cursor_id) {
            Ok(claim) => claim,
            Err(ClaimError::Closing | ClaimError::Closed) => {
                self.closed.store(true, Ordering::Release);
                return;
            }
            Err(ClaimError::Busy) => {
                tracing::warn!(
                    cursor_id = self.cursor_id,
                    operation = "finalize",
                    "PyAsyncCursor finalizer skipped: session busy; prepared handle deferred to connection close"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    cursor_id = self.cursor_id,
                    operation = "finalize",
                    "PyAsyncCursor finalizer could not claim cleanup: {error:?}"
                );
                session_state.abandon_cursor(self.cursor_id);
                return;
            }
        };
        let operation_id = claim.operation_id;
        let finalizer = FinalizerCleanup {
            cleanup: Some(cleanup),
            completion_guard: FinalizerCompletionGuard::new(session_state, self.cursor_id),
        };
        pyo3_async_runtimes::tokio::get_runtime().spawn(in_cursor_operation_span(
            finalizer.run(claim),
            self.cursor_id,
            operation_id,
            "finalize",
            "closing",
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::ffi::CString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use mssql_tds::error::Error;
    use mssql_tds::test_client_support::tds_client_from_tokens_capturing;
    use pyo3::prelude::*;
    use pyo3::types::{PyCFunction, PyDict};
    use tokio::sync::oneshot;

    use super::{FinalizerCompletionGuard, PyAsyncCursor, clear_result_state, map_claim_error};
    use crate::async_description::DescriptionState;
    use crate::async_errors::ProgrammingError;
    use crate::async_fetch::{BufferedResults, BufferedRowSet, FetchState, FetchStatus};
    use crate::async_handle::LogicalConnection;
    use crate::async_session::{AsyncConnectionState, ClaimError, ConnectionLifecycle};
    use crate::row_writer::PyRowWriter;

    fn cursor() -> (PyAsyncCursor, Arc<std::sync::Mutex<Vec<u8>>>) {
        let (client, transport) = tds_client_from_tokens_capturing(Vec::new());
        let connection = Arc::new(LogicalConnection::new(client));
        let cursor = PyAsyncCursor::new(
            connection.clone(),
            None,
            Arc::new(AtomicBool::new(true)),
            connection.state.clone(),
            1,
            0,
        );
        (cursor, transport)
    }

    fn assert_queued_operation_rejected(operation: &str, closed: bool) {
        let (cursor, transport) = cursor();
        let connection = cursor.tds_client.clone();
        let state = cursor.session_state.clone();
        let runtime = pyo3_async_runtimes::tokio::get_runtime();
        let held_connection = connection.clone();
        let (locked_tx, locked_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let holding = runtime.spawn(async move {
            let operation_id = held_connection.state.claim_connection_operation().unwrap();
            let held = held_connection.lock_operation(operation_id).await.unwrap();
            held_connection.state.release_operation(operation_id);
            locked_tx.send(()).unwrap();
            release_rx.await.unwrap();
            drop(held);
        });
        runtime.block_on(locked_rx).unwrap();
        if !matches!(operation, "execute" | "executemany") {
            let claim = state.claim_execute(1).unwrap();
            state.finish_execute(claim.operation_id, true);
            cursor.fetch_state.set(FetchStatus::Ready);
        }

        Python::attach(|py| {
            let globals = PyDict::new(py);
            globals
                .set_item("cursor", Py::new(py, cursor).unwrap())
                .unwrap();
            let release = std::sync::Mutex::new(Some(release_tx));
            let revoke = PyCFunction::new_closure(py, None, None, move |_, _| {
                if closed {
                    connection.state.mark_closed();
                } else {
                    connection.state.begin_close();
                }
                release.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok::<(), PyErr>(())
            })
            .unwrap();
            globals.set_item("revoke", revoke).unwrap();
            let call = match operation {
                "execute" => "cursor.execute('SELECT 1', use_prepare=False)".to_string(),
                "executemany" => {
                    "cursor.executemany('SELECT ?', [(1,)], use_prepare=False)".to_string()
                }
                _ => format!("cursor.{operation}()"),
            };
            let expected = if closed { "closed" } else { "closing" };
            let code = CString::new(format!(
                r#"import asyncio
async def run():
    pending = {call}
    revoke()
    try:
        await asyncio.wait_for(pending, 5)
    except RuntimeError as error:
        assert str(error) == 'Connection is {expected}', str(error)
    else:
        raise AssertionError('queued {operation} reached the client')
    assert await cursor.close() is None
asyncio.run(run())
"#
            ))
            .unwrap();
            py.run(&code, Some(&globals), None).unwrap();
        });
        runtime.block_on(holding).unwrap();
        assert_eq!(
            state.lifecycle(),
            if closed {
                ConnectionLifecycle::Closed
            } else {
                ConnectionLifecycle::Closing
            }
        );
        assert!(transport.lock().unwrap().is_empty());
    }

    #[test]
    fn queued_execute_and_executemany_reject_revoked_parent() {
        for operation in ["execute", "executemany"] {
            for closed in [false, true] {
                assert_queued_operation_rejected(operation, closed);
            }
        }
    }

    #[test]
    fn queued_fetch_variants_and_nextset_reject_revoked_parent() {
        for operation in ["fetchone", "fetchmany", "fetchall", "nextset"] {
            for closed in [false, true] {
                assert_queued_operation_rejected(operation, closed);
            }
        }
    }

    #[tokio::test]
    async fn admitted_cleanup_after_parent_revocation_clears_only_local_results() {
        for closed in [false, true] {
            let (cursor, transport) = cursor();
            cursor.fetch_state.set(FetchStatus::Ready);
            cursor
                .buffered_results
                .replace(VecDeque::from([BufferedRowSet {
                    metadata: Vec::new(),
                    rows: VecDeque::from([PyRowWriter::new(0)]),
                }]));
            let state = cursor.session_state.clone();
            let claim = state.claim_cursor_close(1).unwrap();
            if closed {
                drop(cursor.tds_client.take_for_close().await.unwrap());
                state.mark_closed();
            } else {
                state.begin_close();
            }

            cursor.cleanup().run(claim).await.unwrap();

            assert!(cursor.closed.load(Ordering::Acquire));
            assert!(cursor.fetch_state.status() == FetchStatus::NoResultSet);
            assert!(!cursor.buffered_results.has_current());
            assert!(transport.lock().unwrap().is_empty());
            assert_eq!(Arc::strong_count(&transport), if closed { 1 } else { 2 });
        }
    }

    #[tokio::test]
    async fn admitted_cleanup_rejects_broken_or_replaced_claim_without_io() {
        for broken in [false, true] {
            let (cursor, transport) = cursor();
            let state = cursor.session_state.clone();
            let claim = state.claim_cursor_close(1).unwrap();
            let replacement = if broken {
                state.mark_broken();
                None
            } else {
                state.release_operation(claim.operation_id);
                Some(state.claim_connection_operation().unwrap())
            };

            let error = cursor.cleanup().run(claim).await.unwrap_err();

            assert!(matches!(error, Error::UsageError(message) if message ==
                format!("Cursor cleanup rejected: {}", if broken { "Broken" } else { "Busy" })));
            assert!(!cursor.closed.load(Ordering::Acquire));
            assert!(transport.lock().unwrap().is_empty());
            if let Some(replacement) = replacement {
                assert!(cursor.tds_client.lock_operation(replacement).await.is_ok());
                state.release_operation(replacement);
            } else {
                assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
            }
        }
    }

    #[test]
    fn clear_result_state_discards_buffered_results() {
        let fetch_state = FetchState::new();
        fetch_state.set(FetchStatus::Ready);
        let description_state = DescriptionState::new();
        let buffered_results = BufferedResults::default();
        buffered_results.replace(VecDeque::from([BufferedRowSet {
            metadata: Vec::new(),
            rows: VecDeque::from([PyRowWriter::new(0)]),
        }]));
        assert!(buffered_results.has_current());

        clear_result_state(&fetch_state, &description_state, &buffered_results);

        assert!(!buffered_results.has_current());
        assert!(fetch_state.status() == FetchStatus::NoResultSet);
    }

    #[test]
    fn no_result_set_claim_maps_to_programming_error() {
        let error = map_claim_error(ClaimError::NoResultSet);

        Python::attach(|py| assert!(error.is_instance_of::<ProgrammingError>(py)));
        assert!(error.to_string().contains("No active result set"));
    }

    #[test]
    fn completed_finalizer_preserves_settled_session() {
        let state = Arc::new(AsyncConnectionState::new());
        let claim = state.claim_cursor_close(1).unwrap();
        let mut guard = FinalizerCompletionGuard::new(Arc::clone(&state), 1);

        state.release_operation(claim.operation_id);
        guard.complete();
        drop(guard);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Open);
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn interrupted_finalizer_breaks_session() {
        let state = Arc::new(AsyncConnectionState::new());
        let _claim = state.claim_cursor_close(1).unwrap();
        let guard = FinalizerCompletionGuard::new(Arc::clone(&state), 1);

        drop(guard);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Broken);
    }
}

#[pymethods]
impl PyAsyncCursor {
    /// Query timeout (seconds) snapshotted from the parent connection. `0` means no timeout.
    #[getter]
    fn timeout(&self) -> u32 {
        self.default_query_timeout
    }

    /// A seven-item DB-API descriptor for each column in the current result set.
    #[getter]
    fn description<'py>(&self, py: Python<'py>) -> Option<Bound<'py, pyo3::types::PyTuple>> {
        self.description_state.get(py)
    }

    /// Number of rows affected by the most recent operation, or `-1` when unknown.
    #[getter]
    fn rowcount(&self) -> i64 {
        self.rowcount.load(Ordering::Acquire)
    }

    /// Number of rows requested by `fetchmany()` when no size is supplied.
    #[getter]
    fn arraysize(&self) -> isize {
        self.arraysize
    }

    /// Set the default number of rows requested by `fetchmany()`.
    #[setter]
    fn set_arraysize(&mut self, arraysize: isize) {
        self.arraysize = arraysize;
    }

    /// Set SQL type, size, and scale hints for the next successful `execute()`.
    ///
    /// Each item is a SQL type integer or `(sql_type, size, decimal_digits)`.
    /// Hints are consumed only after an execution is successfully dispatched.
    fn setinputsizes(&mut self, sizes: &Bound<'_, PyAny>) -> PyResult<()> {
        crate::async_execute::set_input_sizes(self, sizes)
    }

    /// Execute T-SQL and return an awaitable resolving to this cursor.
    ///
    /// Positional parameters use `?` markers. A single mapping uses
    /// `%(name)s` markers. `use_prepare=False` skips prepared execution;
    /// `reset_cursor=False` permits reuse of a compatible prepared statement.
    #[pyo3(signature = (operation, *parameters, use_prepare=true, reset_cursor=true))]
    fn execute<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        operation: String,
        parameters: &Bound<'_, PyTuple>,
        use_prepare: bool,
        reset_cursor: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        crate::async_execute::execute(slf, py, operation, parameters, use_prepare, reset_cursor)
    }

    /// Execute T-SQL once for each parameter row and return this cursor.
    ///
    /// Rows execute sequentially after the complete input iterable is validated.
    /// Positional rows use `?`; mapping rows use `%(name)s`. A SQL error stops
    /// execution and reports its zero-based parameter-row index. Earlier rows may
    /// already be committed when autocommit is enabled; an explicit transaction
    /// remains open for the caller to commit or roll back.
    ///
    /// DML row counts are aggregated. The aggregate is `-1` if any surfaced
    /// statement result has an unknown row count. Row-producing results also set
    /// `rowcount` to `-1`, are buffered, and retain their boundaries for
    /// `fetch*()` and `nextset()`. The query timeout applies separately to each execution.
    /// Peak memory scales with the complete parameter input plus all buffered
    /// result rows; the input iterable is not streamed during execution.
    #[pyo3(signature = (operation, seq_of_parameters, *, use_prepare=true))]
    fn executemany<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        operation: String,
        seq_of_parameters: &Bound<'_, PyAny>,
        use_prepare: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        crate::async_execute::executemany(slf, py, operation, seq_of_parameters, use_prepare)
    }

    /// Fetch the next row and return an awaitable resolving to a tuple or `None`.
    fn fetchone<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        crate::async_fetch::fetchone(slf, py)
    }

    /// Fetch at most `size` rows, defaulting to `arraysize`.
    #[pyo3(signature = (size=None))]
    fn fetchmany<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        size: Option<isize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let size = size.unwrap_or_else(|| slf.borrow(py).arraysize);
        crate::async_fetch::fetchmany(slf, py, size)
    }

    /// Fetch all remaining rows in the current result set.
    fn fetchall<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        crate::async_fetch::fetchall(slf, py)
    }

    /// Advance to the next statement result, returning `True` or `False` at batch end.
    fn nextset<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        crate::async_fetch::nextset(slf, py)
    }

    /// Drain pending results, release prepared handles, and close this cursor.
    ///
    /// Returns an awaitable resolving to `None`. Closing an already closed
    /// cursor is a no-op.
    fn close<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let (cleanup, cleanup_started, dispatch) = {
            let cursor = slf.borrow(py);
            if cursor.closed.load(Ordering::Acquire)
                || cursor
                    .cleanup_started
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                return pyo3_async_runtimes::tokio::future_into_py(py, async move {
                    Python::attach(|py| Ok(py.None()))
                });
            }
            (
                cursor.cleanup(),
                cursor.cleanup_started.clone(),
                cursor.tracing_dispatch.clone(),
            )
        };
        let session_state = cleanup.session_state.clone();
        let claim = match session_state.claim_cursor_close(cleanup.cursor_id) {
            Ok(claim) => claim,
            Err(ClaimError::Closing | ClaimError::Closed) => {
                cleanup.clear_result_state();
                cleanup.closed.store(true, Ordering::Release);
                return pyo3_async_runtimes::tokio::future_into_py(py, async move {
                    Python::attach(|py| Ok(py.None()))
                });
            }
            Err(error) => {
                cleanup_started.store(false, Ordering::Release);
                return Err(map_claim_error(error));
            }
        };
        let operation_id = claim.operation_id;
        let cursor_id = cleanup.cursor_id;
        let future = async move {
            cleanup.run(claim).await.map_err(|error| {
                record_result_set_status("error");
                tracing::error!("PyAsyncCursor::close: failed: {error}");
                PyRuntimeError::new_err(format!("Cursor close failed: {error}"))
            })?;
            record_result_set_status("closed");
            Python::attach(|py| Ok(py.None()))
        };
        let future = in_cursor_operation_span(future, cursor_id, operation_id, "close", "closing");
        let future = async move {
            match dispatch {
                Some(dispatch) => future.with_subscriber(dispatch).await,
                None => future.await,
            }
        };
        match pyo3_async_runtimes::tokio::future_into_py(py, future) {
            Ok(awaitable) => Ok(awaitable),
            Err(error) => {
                session_state.release_operation(operation_id);
                cleanup_started.store(false, Ordering::Release);
                Err(error)
            }
        }
    }
}

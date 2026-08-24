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
use std::sync::atomic::{AtomicBool, Ordering};

use mssql_tds::connection::tds_client::{
    ExecuteOptions, PreparedStatement, StatementId, StatementResult,
};
use mssql_tds::error::Error;
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use mssql_tds::connection::tds_client::TdsClient;

use crate::async_parameters::{ParameterMetadata, bind_parameters, parse_input_sizes};
use crate::async_session::{
    AsyncConnectionState, ClaimError, CursorCloseClaim, CursorId, ExecuteClaim, OperationId,
};

/// Cursor-local state for prepared execution and deferred handle cleanup.
#[derive(Default)]
struct PreparedState {
    /// The current prepared statement and its optional live server handle.
    statement: Option<PreparedStatement>,
    /// Metadata shape used to determine whether the statement must be rebound.
    parameter_signature: Vec<ParameterMetadata>,
    /// A superseded statement retained until its unprepare request is serialized.
    orphaned: Option<StatementId>,
}

/// Whether execute must replace the cursor's current prepared statement.
fn should_replace_prepared_statement(
    state: &PreparedState,
    operation: &str,
    parameter_signature: &[ParameterMetadata],
    reset_cursor: bool,
) -> bool {
    reset_cursor
        || state
            .statement
            .as_ref()
            .is_none_or(|statement| statement.sql() != operation)
        || state.parameter_signature != parameter_signature
}

/// Python-free inputs required to execute one command on the TDS session.
struct ExecuteRequest {
    operation: String,
    rpc_parameters: Vec<RpcParameter>,
    parameter_signature: Vec<ParameterMetadata>,
    use_prepare: bool,
    reset_cursor: bool,
    timeout: u32,
    autocommit: bool,
}

/// Cursor state captured under the GIL before constructing the execute future.
struct ExecuteSnapshot {
    request: ExecuteRequest,
    client: Arc<Mutex<TdsClient>>,
    dispatch: Option<tracing::Dispatch>,
    prepared_state: Arc<Mutex<PreparedState>>,
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    input_sizes_generation: u64,
    cleanup_required: Arc<AtomicBool>,
}

/// Ownership state left by a successful command dispatch.
enum ExecuteOutcome {
    Idle,
    Fetching,
}

impl ExecuteOutcome {
    fn has_open_batch(&self) -> bool {
        matches!(self, Self::Fetching)
    }
}

/// Protocol failure and whether ownership must poison the connection.
struct ExecuteFailure {
    error: Error,
    break_connection: bool,
}

impl ExecuteFailure {
    fn broken(error: Error) -> Self {
        Self {
            error,
            break_connection: true,
        }
    }
}

impl From<Error> for ExecuteFailure {
    fn from(error: Error) -> Self {
        Self {
            error,
            break_connection: false,
        }
    }
}

/// Runs the protocol portion of execute without accessing Python objects.
async fn execute_on_client(
    client: &mut TdsClient,
    prepared_state: &Mutex<PreparedState>,
    claim: &ExecuteClaim,
    request: ExecuteRequest,
) -> Result<ExecuteOutcome, ExecuteFailure> {
    let ExecuteRequest {
        operation,
        rpc_parameters,
        parameter_signature,
        use_prepare,
        reset_cursor,
        timeout,
        autocommit,
    } = request;

    if claim.drain_previous {
        client.close_query().await?;
    }
    let options = ExecuteOptions {
        timeout: if timeout == 0 { None } else { Some(timeout) },
        cancel: Some(&claim.cancel_handle),
        ..Default::default()
    };
    if !autocommit && !client.has_active_transaction() {
        // TODO(mssql-tds): Add an options-aware begin_transaction API that applies
        // reconnect timeout accounting and cancellation, and records whether the
        // transaction-manager request reached the wire. Until then, any BEGIN
        // failure must conservatively poison the session.
        client
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .map_err(ExecuteFailure::broken)?;
    }

    let first = if use_prepare {
        // TODO(performance): Benchmark prepared-statement reuse independently
        // from placeholder scanning and scalar conversion.
        let mut state = prepared_state.lock().await;
        let replace_statement = should_replace_prepared_statement(
            &state,
            &operation,
            &parameter_signature,
            reset_cursor,
        );
        if replace_statement {
            if let Some(mut statement) = state.statement.take()
                && let Some(statement_id) = statement.take_id()
            {
                state.orphaned = Some(statement_id);
            }
            state.statement = Some(PreparedStatement::new(operation));
            state.parameter_signature = parameter_signature;
        }
        let PreparedState {
            statement,
            parameter_signature: _,
            orphaned,
        } = &mut *state;
        client
            .execute_prepared(
                statement
                    .as_mut()
                    .expect("prepared statement was initialized"),
                rpc_parameters,
                orphaned,
                options,
            )
            .await?
    } else if rpc_parameters.is_empty() {
        client.execute(operation, options).await?
    } else {
        client
            .execute_sp_executesql(operation, rpc_parameters, options)
            .await?
    };
    if !matches!(first, StatementResult::Rows) {
        client.advance_to_rows().await?;
    }
    Ok(if client.has_open_batch() {
        ExecuteOutcome::Fetching
    } else {
        ExecuteOutcome::Idle
    })
}

/// Converts a failed session claim into a Python error with operation-specific busy text.
fn map_claim_error_with_busy_message(error: ClaimError, busy_message: &'static str) -> PyErr {
    match error {
        ClaimError::Closing => PyRuntimeError::new_err("Connection is closing"),
        ClaimError::Closed => PyRuntimeError::new_err("Connection is closed"),
        ClaimError::Broken => PyRuntimeError::new_err("Connection is broken"),
        ClaimError::Busy => PyRuntimeError::new_err(busy_message),
    }
}

/// Converts a failed cursor operation claim into a Python error.
fn map_claim_error(error: ClaimError) -> PyErr {
    map_claim_error_with_busy_message(error, "Connection is busy with another cursor operation")
}

/// Logs a TDS execution failure and exposes it as a Python runtime error.
fn map_execute_error(error: impl std::fmt::Display) -> PyErr {
    tracing::error!("PyAsyncCursor::execute: failed: {error}");
    PyRuntimeError::new_err(format!("Query execution failed: {error}"))
}

/// Releases an execute or close claim and poisons the session if the future is interrupted.
struct ExecuteGuard {
    session_state: Arc<AsyncConnectionState>,
    operation_id: OperationId,
    completed: bool,
}

impl ExecuteGuard {
    /// Guards an operation claim already acquired from the session state.
    fn new(session_state: Arc<AsyncConnectionState>, operation_id: OperationId) -> Self {
        Self {
            session_state,
            operation_id,
            completed: false,
        }
    }

    /// Completes execution and transitions ownership to fetching or idle.
    fn complete(&mut self, outcome: &ExecuteOutcome) {
        self.session_state
            .finish_execute(self.operation_id, outcome.has_open_batch());
        self.completed = true;
    }

    /// Releases ownership after an error that left the protocol reusable.
    fn release(&mut self) {
        self.session_state.release_operation(self.operation_id);
        self.completed = true;
    }

    /// Marks the connection broken after an error left protocol work pending.
    fn break_connection(&mut self) {
        self.session_state.mark_broken();
        self.session_state.release_operation(self.operation_id);
        self.completed = true;
    }

    /// Settles ownership according to the remaining protocol state.
    fn settle(&mut self, has_open_batch: bool) {
        if has_open_batch {
            self.break_connection();
        } else {
            self.release();
        }
    }
}

impl Drop for ExecuteGuard {
    /// Marks the session broken when cancellation or unwinding interrupts an operation.
    fn drop(&mut self) {
        if !self.completed {
            self.session_state.mark_broken();
            self.session_state.release_operation(self.operation_id);
        }
    }
}

/// Python-independent resources required to drain and release a cursor.
struct CursorCleanup {
    client: Arc<Mutex<TdsClient>>,
    prepared_state: Arc<Mutex<PreparedState>>,
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    timeout: u32,
    closed: Arc<AtomicBool>,
}

impl CursorCleanup {
    async fn run(self, claim: CursorCloseClaim) -> Result<(), Error> {
        let mut cleanup_guard = ExecuteGuard::new(self.session_state.clone(), claim.operation_id);
        let (result, has_open_batch) = {
            let mut client = self.client.lock().await;
            let result = async {
                if claim.drain_previous {
                    client.close_query().await?;
                }
                let statement_ids = {
                    let mut state = self.prepared_state.lock().await;
                    let current = state
                        .statement
                        .as_mut()
                        .and_then(PreparedStatement::take_id);
                    let orphaned = state.orphaned.take();
                    state.statement = None;
                    state.parameter_signature.clear();
                    [current, orphaned]
                };
                let mut released = None;
                for statement_id in statement_ids.into_iter().flatten() {
                    if released == Some(statement_id) {
                        continue;
                    }
                    client
                        .unprepare(
                            statement_id,
                            ExecuteOptions {
                                timeout: Some(self.timeout),
                                ..Default::default()
                            },
                        )
                        .await?;
                    released = Some(statement_id);
                }
                Ok::<(), Error>(())
            }
            .await;
            let has_open_batch = client.has_open_batch();
            (result, has_open_batch)
        };

        cleanup_guard.settle(has_open_batch);
        // Cleanup consumes the close attempt even when draining or unprepare fails.
        self.closed.store(true, Ordering::Release);
        result?;
        Ok(())
    }
}

/// Ensures abandoned finalizer tasks synchronously poison session ownership.
struct FinalizerCleanup {
    cleanup: Option<CursorCleanup>,
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    completed: bool,
}

impl FinalizerCleanup {
    async fn run(mut self, claim: CursorCloseClaim) {
        let cleanup = self.cleanup.take().expect("finalizer cleanup is available");
        match cleanup.run(claim).await {
            Ok(()) => self.completed = true,
            Err(error) => {
                tracing::warn!("PyAsyncCursor finalizer cleanup failed: {error}");
                self.session_state.abandon_cursor(self.cursor_id);
                self.completed = true;
            }
        }
    }
}

impl Drop for FinalizerCleanup {
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
    /// Cloned from the parent `PyAsyncConnection`. The `Arc` keeps the
    /// client alive across cursor and connection lifetimes; the async mutex
    /// serializes wire access across `.await` points.
    #[allow(dead_code)] // Consumed by upcoming async execute/fetch/close APIs.
    tds_client: Arc<Mutex<TdsClient>>,
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
    input_sizes: Option<Vec<crate::types::ParameterHint>>,
    input_sizes_generation: u64,
    cleanup_required: Arc<AtomicBool>,
    cleanup_started: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl PyAsyncCursor {
    /// Construct a new cursor bound to the given TDS client.
    ///
    /// Called only from `PyAsyncConnection::cursor`.
    pub(crate) fn new(
        tds_client: Arc<Mutex<TdsClient>>,
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
            input_sizes: None,
            input_sizes_generation: 0,
            cleanup_required: Arc::new(AtomicBool::new(false)),
            cleanup_started: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn cleanup(&self) -> CursorCleanup {
        CursorCleanup {
            client: self.tds_client.clone(),
            prepared_state: self.prepared_state.clone(),
            session_state: self.session_state.clone(),
            cursor_id: self.cursor_id,
            timeout: self.default_query_timeout,
            closed: self.closed.clone(),
        }
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
                    "PyAsyncCursor finalizer skipped: session busy; prepared handle deferred to connection close"
                );
                return;
            }
            Err(error) => {
                tracing::warn!("PyAsyncCursor finalizer could not claim cleanup: {error:?}");
                session_state.abandon_cursor(self.cursor_id);
                return;
            }
        };
        let finalizer = FinalizerCleanup {
            cleanup: Some(cleanup),
            session_state,
            cursor_id: self.cursor_id,
            completed: false,
        };
        pyo3_async_runtimes::tokio::get_runtime().spawn(finalizer.run(claim));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::should_replace_prepared_statement;
    use super::{ExecuteGuard, ParameterMetadata, PreparedState, PreparedStatement};
    use crate::async_session::{AsyncConnectionState, ClaimError, ConnectionLifecycle};

    fn prepared_state(sql: &str, signature: Vec<ParameterMetadata>) -> PreparedState {
        PreparedState {
            statement: Some(PreparedStatement::new(sql.to_string())),
            parameter_signature: signature,
            orphaned: None,
        }
    }

    #[test]
    fn compatible_prepared_statement_is_reused_when_reset_is_false() {
        let signature = vec![ParameterMetadata::Scalar("int")];
        let state = prepared_state("SELECT @P1", signature.clone());

        assert!(!should_replace_prepared_statement(
            &state,
            "SELECT @P1",
            &signature,
            false,
        ));
    }

    #[test]
    fn prepared_statement_is_replaced_for_each_incompatible_input() {
        let signature = vec![ParameterMetadata::Scalar("int")];
        let state = prepared_state("SELECT @P1", signature.clone());

        assert!(should_replace_prepared_statement(
            &state,
            "SELECT @P1",
            &signature,
            true,
        ));
        assert!(should_replace_prepared_statement(
            &state,
            "SELECT @P1 + 1",
            &signature,
            false,
        ));
        assert!(should_replace_prepared_statement(
            &state,
            "SELECT @P1",
            &[ParameterMetadata::Scalar("bigint")],
            false,
        ));
        assert!(should_replace_prepared_statement(
            &PreparedState::default(),
            "SELECT @P1",
            &signature,
            false,
        ));
    }

    #[test]
    fn handled_error_releases_reusable_session() {
        let state = Arc::new(AsyncConnectionState::new());
        let claim = state.claim_execute(1).unwrap();
        let mut guard = ExecuteGuard::new(Arc::clone(&state), claim.operation_id);

        guard.settle(false);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Open);
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn handled_error_breaks_session_with_open_batch() {
        let state = Arc::new(AsyncConnectionState::new());
        let claim = state.claim_execute(1).unwrap();
        let mut guard = ExecuteGuard::new(Arc::clone(&state), claim.operation_id);

        guard.settle(true);

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

    /// Set SQL type, size, and scale hints for the next successful `execute()`.
    ///
    /// Each item is a SQL type integer or `(sql_type, size, decimal_digits)`.
    /// Hints are consumed only after an execution is successfully dispatched.
    fn setinputsizes(&mut self, sizes: &Bound<'_, PyAny>) -> PyResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Cursor is closed"));
        }
        self.input_sizes = parse_input_sizes(sizes)?;
        self.input_sizes_generation = self.input_sizes_generation.wrapping_add(1);
        Ok(())
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
        let snapshot = {
            let cursor = slf.borrow(py);
            if cursor.closed.load(Ordering::Acquire) {
                return Err(PyRuntimeError::new_err("Cursor is closed"));
            }
            // TODO(async execute preflight): Parameter normalization and Python-to-TDS
            // conversion currently run synchronously under the GIL before the awaitable is
            // returned. Bound or chunk large parameter/TVP conversion so execute does not
            // block the caller's event-loop thread during preflight.
            let (operation, rpc_parameters, parameter_signature) =
                bind_parameters(operation, parameters, cursor.input_sizes.as_deref())?;
            ExecuteSnapshot {
                request: ExecuteRequest {
                    operation,
                    rpc_parameters,
                    parameter_signature,
                    use_prepare,
                    reset_cursor,
                    timeout: cursor.default_query_timeout,
                    autocommit: cursor.autocommit.load(Ordering::Relaxed),
                },
                client: cursor.tds_client.clone(),
                dispatch: cursor.tracing_dispatch.clone(),
                prepared_state: cursor.prepared_state.clone(),
                session_state: cursor.session_state.clone(),
                cursor_id: cursor.cursor_id,
                input_sizes_generation: cursor.input_sizes_generation,
                cleanup_required: cursor.cleanup_required.clone(),
            }
        };
        let ExecuteSnapshot {
            request,
            client,
            dispatch,
            prepared_state,
            session_state,
            cursor_id,
            input_sizes_generation,
            cleanup_required,
        } = snapshot;
        let claim = session_state
            .claim_execute(cursor_id)
            .map_err(map_claim_error)?;
        let operation_id = claim.operation_id;
        let future_state = session_state.clone();

        let future = async move {
            let mut execute_guard = ExecuteGuard::new(future_state, operation_id);
            cleanup_required.store(true, Ordering::Release);
            tracing::info!(
                "PyAsyncCursor::execute: executing query; parameter_count={}, use_prepare={}, reset_cursor={}",
                request.rpc_parameters.len(),
                request.use_prepare,
                request.reset_cursor
            );

            let (result, has_open_batch) = {
                let mut client = client.lock().await;
                let result = execute_on_client(&mut client, &prepared_state, &claim, request).await;
                let has_open_batch = client.has_open_batch();
                (result, has_open_batch)
            };

            match result {
                Ok(outcome) => {
                    execute_guard.complete(&outcome);
                    Python::attach(|py| {
                        let mut cursor = slf.borrow_mut(py);
                        if cursor.input_sizes_generation == input_sizes_generation {
                            cursor.input_sizes = None;
                        }
                    });
                    tracing::info!("PyAsyncCursor::execute: query executed successfully");
                    Ok(slf)
                }
                Err(error) => {
                    execute_guard.settle(error.break_connection || has_open_batch);
                    Err(map_execute_error(error.error))
                }
            }
        };
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
                Err(error)
            }
        }
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
        let future = async move {
            cleanup.run(claim).await.map_err(|error| {
                tracing::error!("PyAsyncCursor::close: failed: {error}");
                PyRuntimeError::new_err(format!("Cursor close failed: {error}"))
            })?;
            Python::attach(|py| Ok(py.None()))
        };
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

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
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use mssql_tds::connection::tds_client::TdsClient;

use crate::async_parameters::{bind_parameters, parse_input_sizes};
use crate::async_session::{AsyncConnectionState, ClaimError, CursorId, OperationId};

#[derive(Default)]
struct PreparedState {
    statement: Option<PreparedStatement>,
    parameter_signature: Vec<String>,
    orphaned: Option<StatementId>,
}

fn map_claim_error(error: ClaimError) -> PyErr {
    match error {
        ClaimError::Closing => PyRuntimeError::new_err("Connection is closing"),
        ClaimError::Closed => PyRuntimeError::new_err("Connection is closed"),
        ClaimError::Broken => PyRuntimeError::new_err("Connection is broken"),
        ClaimError::Busy => {
            PyRuntimeError::new_err("Connection is busy with another cursor operation")
        }
    }
}

fn map_execute_error(error: impl std::fmt::Display) -> PyErr {
    tracing::error!("PyAsyncCursor::execute: failed: {error}");
    PyRuntimeError::new_err(format!("Query execution failed: {error}"))
}

struct ExecuteGuard {
    session_state: Arc<AsyncConnectionState>,
    operation_id: OperationId,
    completed: bool,
}

impl ExecuteGuard {
    fn new(session_state: Arc<AsyncConnectionState>, operation_id: OperationId) -> Self {
        Self {
            session_state,
            operation_id,
            completed: false,
        }
    }

    fn complete(&mut self, has_open_batch: bool) {
        self.session_state
            .finish_execute(self.operation_id, has_open_batch);
        self.completed = true;
    }

    fn fail(&mut self) {
        self.session_state.release_operation(self.operation_id);
        self.completed = true;
    }
}

impl Drop for ExecuteGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.session_state.mark_broken();
            self.session_state.release_operation(self.operation_id);
        }
    }
}

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

    /// Set one-shot SQL type, size, and scale hints for the next successful execute.
    fn setinputsizes(&mut self, sizes: &Bound<'_, PyAny>) -> PyResult<()> {
        self.input_sizes = parse_input_sizes(sizes)?;
        self.input_sizes_generation = self.input_sizes_generation.wrapping_add(1);
        Ok(())
    }

    /// Prepare and execute a T-SQL operation, resolving to this cursor.
    #[pyo3(signature = (operation, *parameters, use_prepare=true, reset_cursor=true))]
    fn execute<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        operation: String,
        parameters: &Bound<'_, PyTuple>,
        use_prepare: bool,
        reset_cursor: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (
            operation,
            rpc_parameters,
            parameter_signature,
            client,
            dispatch,
            prepared_state,
            autocommit,
            session_state,
            cursor_id,
            timeout,
            input_sizes_generation,
        ) = {
            let cursor = slf.borrow(py);
            // TODO(async execute preflight): Parameter normalization and Python-to-TDS
            // conversion currently run synchronously under the GIL before the awaitable is
            // returned. Bound or chunk large parameter/TVP conversion so execute does not
            // block the caller's event-loop thread during preflight.
            let (operation, rpc_parameters, parameter_signature) =
                bind_parameters(operation, parameters, cursor.input_sizes.as_deref())?;
            (
                operation,
                rpc_parameters,
                parameter_signature,
                cursor.tds_client.clone(),
                cursor.tracing_dispatch.clone(),
                cursor.prepared_state.clone(),
                cursor.autocommit.load(Ordering::Relaxed),
                cursor.session_state.clone(),
                cursor.cursor_id,
                cursor.default_query_timeout,
                cursor.input_sizes_generation,
            )
        };
        let claim = session_state
            .claim_execute(cursor_id)
            .map_err(map_claim_error)?;
        let operation_id = claim.operation_id;
        let future_state = session_state.clone();

        let future = async move {
            let mut execute_guard = ExecuteGuard::new(future_state, operation_id);
            tracing::info!(
                "PyAsyncCursor::execute: executing query; parameter_count={}, use_prepare={}, reset_cursor={}",
                rpc_parameters.len(),
                use_prepare,
                reset_cursor
            );

            let result = async {
                let mut client = client.lock().await;
                if claim.drain_previous {
                    client.close_query().await?;
                }
                if !autocommit && !client.has_active_transaction() {
                    client
                        .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
                        .await?;
                }

                let options = ExecuteOptions {
                    timeout: Some(timeout),
                    cancel: Some(&claim.cancel_handle),
                    ..Default::default()
                };
                let first = if use_prepare {
                    let mut state = prepared_state.lock().await;
                    let replace_statement = reset_cursor
                        || state
                            .statement
                            .as_ref()
                            .is_none_or(|statement| statement.sql() != operation)
                        || state.parameter_signature != parameter_signature;
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
                Ok::<bool, mssql_tds::error::Error>(client.has_open_batch())
            }
            .await;

            match result {
                Ok(has_open_batch) => {
                    execute_guard.complete(has_open_batch);
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
                    execute_guard.fail();
                    Err(map_execute_error(error))
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

    // TODO(mssql-tds blockers for async execute parity):
    // - Add a public, general TdsClient parameter-description API backed by
    //   sp_describe_undeclared_parameters. The existing describe path is
    //   private and specific to Always Encrypted, so an unhinted Python None
    //   still falls back to NVARCHAR(1).
    // - Add declaration metadata to nullable Decimal/Numeric RPC parameters.
    //   RpcParameter/SqlType cannot carry precision and scale separately from
    //   a non-NULL DecimalParts value, and declaration generation hardcodes
    //   SqlType::{Decimal, Numeric}(None) to (18,10).
    // - Add a public SqlType::Udt input contract and RPC serializer carrying
    //   database, schema, and server UDT type names. mssql-tds currently parses
    //   UDT result metadata internally but cannot send a typed UDT parameter.
    // TODO(remaining async transaction work):
    // - Add an awaitable connection mode-change API rather than a synchronous
    //   property setter: false -> true commits active work before changing
    //   mode and leaves the mode unchanged if commit fails; true -> false
    //   defers begin until the next execute.
    // - Test lazy begin, restart after commit/rollback, both mode transitions,
    //   context finalization, and cleanup-error precedence.
    //
    // TODO(remaining async fetch/close/cancel ownership work):
    // - Release Fetching ownership when results are exhausted or the cursor is
    //   closed.
    // - Add cursor cancel(), triggering the root CancelHandle only when cursor
    //   and operation IDs match; complete ATTENTION cleanup before deciding
    //   whether the connection remains reusable or must be marked Broken.
    // - Add a two-cursor acceptance test with a multi-packet result: reject
    //   cursor B with a typed busy-state error while cursor A has unread rows,
    //   verify A can drain its remaining rows, then verify B can execute after
    //   A drains or closes.
}

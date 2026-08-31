// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous command execution for [`crate::async_cursor::PyAsyncCursor`].

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use mssql_tds::connection::tds_client::{
    ExecuteOptions, PreparedStatement, StatementId, StatementResult, TdsClient,
};
use mssql_tds::error::{Error, TimeoutErrorType};
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use crate::async_cursor::{PyAsyncCursor, map_claim_error};
use crate::async_fetch::{FetchState, FetchStatus};
use crate::async_parameters::{
    ParameterBindingPlan, ParameterMetadata, bind_parameters, parse_input_sizes,
};
use crate::async_session::{
    AsyncConnectionState, CursorId, ExecuteClaim, SessionOperationGuard, SessionPreflightGuard,
};
use crate::types::ParameterHint;

/// Cursor-local state for prepared execution and deferred handle cleanup.
#[derive(Default)]
pub(crate) struct PreparedState {
    statement: Option<PreparedStatement>,
    parameter_signature: Vec<ParameterMetadata>,
    orphaned: Option<StatementId>,
}

impl PreparedState {
    pub(crate) fn take_statement_ids(&mut self) -> [Option<StatementId>; 2] {
        let current = self.statement.as_mut().and_then(PreparedStatement::take_id);
        let orphaned = self.orphaned.take();
        self.statement = None;
        self.parameter_signature.clear();
        [current, orphaned]
    }
}

pub(crate) async fn release_prepared_statements(
    client: &mut TdsClient,
    prepared_state: &Mutex<PreparedState>,
    timeout: u32,
) -> Result<(), Error> {
    let statement_ids = prepared_state.lock().await.take_statement_ids();
    let mut released = None;
    for statement_id in statement_ids.into_iter().flatten() {
        if released == Some(statement_id) {
            continue;
        }
        client
            .unprepare(
                statement_id,
                ExecuteOptions {
                    timeout: Some(timeout),
                    ..Default::default()
                },
            )
            .await?;
        released = Some(statement_id);
    }
    Ok(())
}

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

struct ExecuteRequest {
    operation: String,
    rpc_parameters: Vec<RpcParameter>,
    parameter_signature: Vec<ParameterMetadata>,
    use_prepare: bool,
    reset_cursor: bool,
    timeout: u32,
    autocommit: bool,
}

pub(crate) struct ExecuteResources {
    client: Arc<Mutex<TdsClient>>,
    dispatch: Option<tracing::Dispatch>,
    prepared_state: Arc<Mutex<PreparedState>>,
    autocommit: bool,
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    timeout: u32,
    input_sizes: Option<Vec<ParameterHint>>,
    input_sizes_generation: u64,
    cleanup_required: Arc<AtomicBool>,
    fetch_state: Arc<FetchState>,
    rowcount: Arc<AtomicI64>,
}

impl ExecuteResources {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: Arc<Mutex<TdsClient>>,
        dispatch: Option<tracing::Dispatch>,
        prepared_state: Arc<Mutex<PreparedState>>,
        autocommit: bool,
        session_state: Arc<AsyncConnectionState>,
        cursor_id: CursorId,
        timeout: u32,
        input_sizes: Option<Vec<ParameterHint>>,
        input_sizes_generation: u64,
        cleanup_required: Arc<AtomicBool>,
        fetch_state: Arc<FetchState>,
        rowcount: Arc<AtomicI64>,
    ) -> Self {
        Self {
            client,
            dispatch,
            prepared_state,
            autocommit,
            session_state,
            cursor_id,
            timeout,
            input_sizes,
            input_sizes_generation,
            cleanup_required,
            fetch_state,
            rowcount,
        }
    }
}

enum ExecuteOutcome {
    Idle,
    Fetching,
}

impl ExecuteOutcome {
    fn has_open_batch(&self) -> bool {
        matches!(self, Self::Fetching)
    }
}

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

async fn execute_on_client(
    client: &mut TdsClient,
    prepared_state: &Mutex<PreparedState>,
    claim: &ExecuteClaim,
    drain_previous: bool,
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

    if drain_previous {
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

fn map_execute_error(error: impl std::fmt::Display) -> PyErr {
    tracing::debug!("PyAsyncCursor::execute: failed: {error}");
    PyRuntimeError::new_err(format!("Query execution failed: {error}"))
}

fn clear_input_sizes_if_current(cursor: &Py<PyAsyncCursor>, generation: u64) {
    Python::attach(|py| {
        let mut cursor = cursor.borrow_mut(py);
        if cursor.input_sizes_generation() == generation {
            cursor.clear_input_sizes();
        }
    });
}

async fn with_dispatch<F>(dispatch: Option<tracing::Dispatch>, future: F) -> F::Output
where
    F: Future,
{
    match dispatch {
        Some(dispatch) => future.with_subscriber(dispatch).await,
        None => future.await,
    }
}

struct ExecuteManyRequest {
    operation: String,
    rows: Vec<ExecuteManyRow>,
    use_prepare: bool,
    timeout: u32,
    autocommit: bool,
}

struct ExecuteManyRow {
    rpc_parameters: Vec<RpcParameter>,
    parameter_signature: Vec<ParameterMetadata>,
}

struct PreflightWorkGuard {
    cancelled: Arc<AtomicBool>,
    completed: bool,
}

fn has_stable_signature(rows: &[ExecuteManyRow]) -> bool {
    rows.first().is_none_or(|first| {
        rows.iter()
            .all(|row| row.parameter_signature == first.parameter_signature)
    })
}

impl PreflightWorkGuard {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for PreflightWorkGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

fn bind_parameter_rows(
    operation: String,
    seq_of_parameters: Py<PyAny>,
    hints: Option<&[ParameterHint]>,
    cancelled: &AtomicBool,
) -> PyResult<(String, Vec<ExecuteManyRow>)> {
    let mut plan = None;
    let mut rows = Vec::new();
    let iterator = Python::attach(|py| seq_of_parameters.bind(py).try_iter().map(Bound::unbind))?;
    for row_index in 0_usize.. {
        if cancelled.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err(
                "ExecuteMany preflight was cancelled",
            ));
        }
        let row = Python::attach(|py| {
            let mut iterator = iterator.bind(py).clone();
            let Some(row) = iterator.next() else {
                return Ok(None);
            };
            let row = row?;
            let is_named = row.cast::<PyDict>().is_ok();
            if plan.is_none() {
                plan = Some(ParameterBindingPlan::new(operation.clone(), is_named)?);
            }
            let plan = plan.as_ref().expect("parameter plan was initialized");
            if plan.named() != is_named {
                return Err(PyTypeError::new_err(format!(
                    "ExecuteMany parameter row {row_index} uses a different parameter style"
                )));
            }
            let (parameters, signature) = plan.bind_row(&row, hints).map_err(|error| {
                if error.is_instance_of::<PyTypeError>(py) {
                    PyTypeError::new_err(format!("ExecuteMany parameter row {row_index}: {error}"))
                } else {
                    error
                }
            })?;
            Ok(Some(ExecuteManyRow {
                rpc_parameters: parameters,
                parameter_signature: signature,
            }))
        })?;
        match row {
            Some(row) => rows.push(row),
            None => break,
        }
    }
    Ok((
        plan.map_or(operation, ParameterBindingPlan::into_operation),
        rows,
    ))
}

fn remaining_timeout(timeout: u32, started: Instant) -> Result<u32, Error> {
    if timeout == 0 {
        return Ok(0);
    }
    let limit = Duration::from_secs(u64::from(timeout));
    let remaining = limit.checked_sub(started.elapsed()).ok_or_else(|| {
        Error::TimeoutError(TimeoutErrorType::String(
            "ExecuteMany exceeded the configured query timeout".to_string(),
        ))
    })?;
    Ok(u32::try_from(remaining.as_secs().max(1)).unwrap_or(u32::MAX))
}

async fn executemany_on_client(
    client: &mut TdsClient,
    prepared_state: &Mutex<PreparedState>,
    claim: &ExecuteClaim,
    request: ExecuteManyRequest,
) -> Result<i64, ExecuteFailure> {
    let ExecuteManyRequest {
        operation,
        rows,
        use_prepare,
        timeout,
        autocommit,
    } = request;
    let started = Instant::now();
    let mut total = 0_i64;

    if claim.drain_previous {
        client.close_query().await?;
    }

    for (row_index, row) in rows.into_iter().enumerate() {
        let row_timeout = remaining_timeout(timeout, started)?;
        let outcome = execute_on_client(
            client,
            prepared_state,
            claim,
            false,
            ExecuteRequest {
                operation: operation.clone(),
                rpc_parameters: row.rpc_parameters,
                parameter_signature: row.parameter_signature,
                use_prepare,
                reset_cursor: row_index == 0,
                timeout: row_timeout,
                autocommit,
            },
        )
        .await?;
        if outcome.has_open_batch() {
            client.close_query().await?;
            return Err(Error::UsageError(
                "ExecuteMany does not yet support operations that return rows".to_string(),
            )
            .into());
        }
        let affected = client.last_rows_affected();
        if affected < 0 {
            total = -1;
        } else if total >= 0 {
            total = total.checked_add(affected).ok_or_else(|| {
                Error::UsageError("ExecuteMany rowcount overflowed i64".to_string())
            })?;
        }
    }
    Ok(total)
}

pub(crate) fn set_input_sizes(
    cursor: &mut PyAsyncCursor,
    sizes: &Bound<'_, PyAny>,
) -> PyResult<()> {
    cursor.replace_input_sizes(parse_input_sizes(sizes)?)
}

pub(crate) fn execute<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
    operation: String,
    parameters: &Bound<'_, PyTuple>,
    use_prepare: bool,
    reset_cursor: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let resources = cursor.borrow(py).execute_resources()?;
    let ExecuteResources {
        client,
        dispatch,
        prepared_state,
        autocommit,
        session_state,
        cursor_id,
        timeout,
        input_sizes,
        input_sizes_generation,
        cleanup_required,
        fetch_state,
        rowcount,
    } = resources;
    // TODO(async execute preflight): Parameter normalization and Python-to-TDS
    // conversion currently run synchronously under the GIL before the awaitable is
    // returned. Bound or chunk large parameter/TVP conversion so execute does not
    // block the caller's event-loop thread during preflight.
    let (operation, rpc_parameters, parameter_signature) =
        bind_parameters(operation, parameters, input_sizes.as_deref())?;
    let request = ExecuteRequest {
        operation,
        rpc_parameters,
        parameter_signature,
        use_prepare,
        reset_cursor,
        timeout,
        autocommit,
    };
    let claim = session_state
        .claim_execute(cursor_id)
        .map_err(map_claim_error)?;
    let operation_id = claim.operation_id;
    let future_state = session_state.clone();
    let previous_fetch_status = fetch_state.replace(FetchStatus::NoResultSet);
    let future_fetch_state = fetch_state.clone();
    let previous_rowcount = rowcount.swap(-1, Ordering::AcqRel);
    let future_rowcount = rowcount.clone();

    let future = async move {
        let mut operation_guard = SessionOperationGuard::new(future_state, operation_id);
        cleanup_required.store(true, Ordering::Release);
        tracing::info!(
            "PyAsyncCursor::execute: executing query; parameter_count={}, use_prepare={}, reset_cursor={}",
            request.rpc_parameters.len(),
            request.use_prepare,
            request.reset_cursor
        );

        let (result, has_open_batch) = {
            let mut client = client.lock().await;
            let result = execute_on_client(
                &mut client,
                &prepared_state,
                &claim,
                claim.drain_previous,
                request,
            )
            .await;
            let has_open_batch = client.has_open_batch();
            if result.is_ok() && !has_open_batch {
                future_rowcount.store(client.last_rows_affected(), Ordering::Release);
            }
            (result, has_open_batch)
        };

        match result {
            Ok(outcome) => {
                operation_guard.finish_execute(outcome.has_open_batch());
                future_fetch_state.set(if outcome.has_open_batch() {
                    FetchStatus::Ready
                } else {
                    FetchStatus::NoResultSet
                });
                clear_input_sizes_if_current(&cursor, input_sizes_generation);
                tracing::info!("PyAsyncCursor::execute: query executed successfully");
                Ok(cursor)
            }
            Err(error) => {
                operation_guard.settle(error.break_connection || has_open_batch);
                Err(map_execute_error(error.error))
            }
        }
    };
    let future = with_dispatch(dispatch, future);

    match pyo3_async_runtimes::tokio::future_into_py(py, future) {
        Ok(awaitable) => Ok(awaitable),
        Err(error) => {
            session_state.release_operation(operation_id);
            fetch_state.set(previous_fetch_status);
            rowcount.store(previous_rowcount, Ordering::Release);
            Err(error)
        }
    }
}

pub(crate) fn executemany<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
    operation: String,
    seq_of_parameters: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let resources = cursor.borrow(py).execute_resources()?;
    let ExecuteResources {
        client,
        dispatch,
        prepared_state,
        autocommit,
        session_state,
        cursor_id,
        timeout,
        input_sizes,
        input_sizes_generation,
        cleanup_required,
        fetch_state,
        rowcount,
    } = resources;
    let seq_of_parameters = seq_of_parameters.clone().unbind();
    let claim = session_state
        .claim_execute(cursor_id)
        .map_err(map_claim_error)?;
    let operation_id = claim.operation_id;
    let future_state = session_state.clone();
    let previous_fetch_status = fetch_state.replace(FetchStatus::NoResultSet);
    let future_fetch_state = fetch_state.clone();
    let previous_rowcount = rowcount.swap(-1, Ordering::AcqRel);
    let future_rowcount = rowcount.clone();

    let future = async move {
        let mut preflight_guard = SessionPreflightGuard::new(future_state.clone(), operation_id);
        let preflight_cancelled = Arc::new(AtomicBool::new(false));
        let mut preflight_work_guard = PreflightWorkGuard::new(preflight_cancelled.clone());
        let request = tokio::task::spawn_blocking(move || {
            let (operation, rows) = bind_parameter_rows(
                operation,
                seq_of_parameters,
                input_sizes.as_deref(),
                &preflight_cancelled,
            )?;
            let use_prepare = has_stable_signature(&rows);
            Ok::<_, PyErr>(ExecuteManyRequest {
                operation,
                rows,
                use_prepare,
                timeout,
                autocommit,
            })
        })
        .await;
        let request = match request {
            Ok(request) => request,
            Err(error) => Err(PyRuntimeError::new_err(format!(
                "ExecuteMany preflight failed: {error}"
            ))),
        };
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                future_state.release_operation(operation_id);
                preflight_guard.complete();
                future_fetch_state.set(previous_fetch_status);
                future_rowcount.store(previous_rowcount, Ordering::Release);
                return Err(error);
            }
        };
        preflight_work_guard.complete();
        preflight_guard.complete();
        let mut operation_guard = SessionOperationGuard::new(future_state, operation_id);
        cleanup_required.store(true, Ordering::Release);
        tracing::info!(
            "PyAsyncCursor::executemany: executing parameter rows; row_count={}",
            request.rows.len()
        );

        let (result, has_open_batch) = {
            let mut client = client.lock().await;
            let result = executemany_on_client(&mut client, &prepared_state, &claim, request).await;
            let has_open_batch = client.has_open_batch();
            (result, has_open_batch)
        };

        match result {
            Ok(affected) => {
                operation_guard.finish_execute(false);
                future_fetch_state.set(FetchStatus::NoResultSet);
                future_rowcount.store(affected, Ordering::Release);
                clear_input_sizes_if_current(&cursor, input_sizes_generation);
                tracing::info!(
                    "PyAsyncCursor::executemany: completed successfully; rowcount={affected}"
                );
                Python::attach(|py| Ok(py.None()))
            }
            Err(error) => {
                operation_guard.settle(error.break_connection || has_open_batch);
                Err(map_execute_error(error.error))
            }
        }
    };
    let future = with_dispatch(dispatch, future);

    match pyo3_async_runtimes::tokio::future_into_py(py, future) {
        Ok(awaitable) => Ok(awaitable),
        Err(error) => {
            session_state.release_operation(operation_id);
            fetch_state.set(previous_fetch_status);
            rowcount.store(previous_rowcount, Ordering::Release);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mssql_tds::connection::tds_client::PreparedStatement;
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::error::Error;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

    use super::{
        ExecuteFailure, ExecuteManyRow, ParameterMetadata, PreparedState, has_stable_signature,
        should_replace_prepared_statement,
    };
    use crate::async_session::{
        AsyncConnectionState, ClaimError, ConnectionLifecycle, SessionOperationGuard,
    };

    fn prepared_state(sql: &str, signature: Vec<ParameterMetadata>) -> PreparedState {
        PreparedState {
            statement: Some(PreparedStatement::new(sql.to_string())),
            parameter_signature: signature,
            orphaned: None,
        }
    }

    fn execute_many_row(signature: ParameterMetadata) -> ExecuteManyRow {
        ExecuteManyRow {
            rpc_parameters: vec![RpcParameter::new(
                Some("@p1".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(1)),
            )],
            parameter_signature: vec![signature],
        }
    }

    #[test]
    fn executemany_prepares_only_stable_parameter_signatures() {
        assert!(has_stable_signature(&[
            execute_many_row(ParameterMetadata::Scalar("int")),
            execute_many_row(ParameterMetadata::Scalar("int")),
        ]));
        assert!(!has_stable_signature(&[
            execute_many_row(ParameterMetadata::Scalar("tinyint")),
            execute_many_row(ParameterMetadata::Scalar("smallint")),
        ]));
    }

    #[test]
    fn broken_execute_failure_marks_connection_for_breakage() {
        let failure = ExecuteFailure::broken(Error::ProtocolError("BEGIN failed".to_string()));

        assert!(failure.break_connection);
        assert_eq!(failure.error.to_string(), "Protocol Error: BEGIN failed");
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
            true
        ));
        assert!(should_replace_prepared_statement(
            &state,
            "SELECT @P1 + 1",
            &signature,
            false
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
        let mut guard = SessionOperationGuard::new(Arc::clone(&state), claim.operation_id);

        guard.settle(false);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Open);
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn handled_error_breaks_session_with_open_batch() {
        let state = Arc::new(AsyncConnectionState::new());
        let claim = state.claim_execute(1).unwrap();
        let mut guard = SessionOperationGuard::new(Arc::clone(&state), claim.operation_id);

        guard.settle(true);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Broken);
    }
}

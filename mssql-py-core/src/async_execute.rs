// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous command execution for [`crate::async_cursor::PyAsyncCursor`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Instant;

use mssql_tds::connection::tds_client::{
    ExecuteOptions, PreparedStatement, ResultSet, StatementId, StatementResult, TdsClient,
};
use mssql_tds::error::{Error, SqlInfoMessage};
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use mssql_tds::query::metadata::ColumnMetadata;
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyIterator, PyList, PyTuple};
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use crate::async_cursor::{PyAsyncCursor, map_claim_error};
use crate::async_description::{DescriptionState, materialize};
use crate::async_errors::{InternalError, map_tds_error};
use crate::async_fetch::{BufferedResults, BufferedRowSet, FetchState, FetchStatus};
use crate::async_parameters::{
    ParameterBindingPlan, ParameterMetadata, bind_parameters, parse_input_sizes,
};
use crate::async_session::{
    AsyncConnectionState, CursorId, ExecuteClaim, OperationId, SessionOperationGuard,
};
use crate::async_tracing::{in_cursor_operation_span, record_result_set_status};
use crate::row_writer::PyRowWriter;
use crate::types::ParameterHint;

const EXECUTEMANY_PREFLIGHT_CHUNK_SIZE: usize = 256;
const EXECUTEMANY_EXECUTION_YIELD_INTERVAL: usize = 256;

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
    drain_previous: bool,
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
    description_state: Arc<DescriptionState>,
    rowcount: Arc<AtomicI64>,
    buffered_results: Arc<BufferedResults>,
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
        description_state: Arc<DescriptionState>,
        rowcount: Arc<AtomicI64>,
        buffered_results: Arc<BufferedResults>,
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
            description_state,
            rowcount,
            buffered_results,
        }
    }
}

enum ExecuteOutcome {
    NoRows(i64),
    TerminalNoRows(i64),
    Rows(Vec<ColumnMetadata>),
}

impl ExecuteOutcome {
    fn has_open_batch(&self) -> bool {
        !matches!(self, Self::TerminalNoRows(_))
    }

    fn has_rows(&self) -> bool {
        matches!(self, Self::Rows(_))
    }

    fn fetch_status(&self) -> FetchStatus {
        match self {
            Self::NoRows(_) => FetchStatus::NoResultSet,
            Self::TerminalNoRows(_) => FetchStatus::TerminalNoRows,
            Self::Rows(_) => FetchStatus::Ready,
        }
    }

    fn result_set_status(&self) -> &'static str {
        match self {
            Self::NoRows(_) | Self::TerminalNoRows(_) => "no_rows",
            Self::Rows(_) => "rows",
        }
    }

    fn rowcount(&self) -> i64 {
        match self {
            Self::NoRows(rowcount) | Self::TerminalNoRows(rowcount) => *rowcount,
            Self::Rows(_) => -1,
        }
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
        drain_previous,
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
    Ok(match first {
        StatementResult::Rows => ExecuteOutcome::Rows(client.get_metadata().clone()),
        StatementResult::NoRows { rows_affected } if client.has_open_batch() => {
            ExecuteOutcome::NoRows(
                rows_affected
                    .and_then(|count| i64::try_from(count).ok())
                    .unwrap_or(-1),
            )
        }
        StatementResult::NoRows { rows_affected } => ExecuteOutcome::TerminalNoRows(
            rows_affected
                .and_then(|count| i64::try_from(count).ok())
                .unwrap_or(-1),
        ),
        StatementResult::End => ExecuteOutcome::TerminalNoRows(-1),
    })
}

struct ExecuteManyRequest {
    operation: String,
    parameter_sets: Vec<BoundParameterSet>,
    use_prepare: bool,
    timeout: u32,
    autocommit: bool,
}

type BoundParameterSet = (Vec<RpcParameter>, Vec<ParameterMetadata>);

struct ExecuteManyFailure {
    failure: ExecuteFailure,
    row_index: usize,
}

struct ExecuteManyBindingState {
    operation: String,
    iterator: Py<PyIterator>,
    hints: Option<Vec<ParameterHint>>,
    parameter_sets: Vec<BoundParameterSet>,
    plan: Option<ParameterBindingPlan>,
    named: Option<bool>,
}

struct ExecuteManyPreflightGuard {
    cursor_id: CursorId,
    dispatch: Option<tracing::Dispatch>,
    completed: bool,
}

impl ExecuteManyPreflightGuard {
    fn new(cursor_id: CursorId, dispatch: Option<tracing::Dispatch>) -> Self {
        Self {
            cursor_id,
            dispatch,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for ExecuteManyPreflightGuard {
    fn drop(&mut self) {
        if !self.completed {
            let _guard = self.dispatch.as_ref().map(tracing::dispatcher::set_default);
            tracing::warn!(
                cursor_id = self.cursor_id,
                "PyAsyncCursor::executemany: interrupted during parameter preflight; cursor_id={}; connection remains usable",
                self.cursor_id
            );
        }
    }
}

struct ExecuteManyInterruptionGuard {
    cursor_id: CursorId,
    operation_id: OperationId,
    phase: &'static str,
    dispatch: Option<tracing::Dispatch>,
    completed: bool,
}

impl ExecuteManyInterruptionGuard {
    fn new(
        cursor_id: CursorId,
        operation_id: OperationId,
        dispatch: Option<tracing::Dispatch>,
    ) -> Self {
        Self {
            cursor_id,
            operation_id,
            phase: "execution",
            dispatch,
            completed: false,
        }
    }

    fn set_phase(&mut self, phase: &'static str) {
        self.phase = phase;
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for ExecuteManyInterruptionGuard {
    fn drop(&mut self) {
        if !self.completed {
            let _guard = self.dispatch.as_ref().map(tracing::dispatcher::set_default);
            tracing::warn!(
                cursor_id = self.cursor_id,
                operation_id = self.operation_id,
                phase = self.phase,
                "PyAsyncCursor::executemany: interrupted; cursor_id={}; operation_id={}; phase={}; connection marked broken",
                self.cursor_id,
                self.operation_id,
                self.phase
            );
        }
    }
}

async fn execute_many_on_client(
    client: &mut TdsClient,
    prepared_state: &Mutex<PreparedState>,
    claim: &ExecuteClaim,
    request: ExecuteManyRequest,
) -> Result<(i64, VecDeque<BufferedRowSet>), ExecuteManyFailure> {
    let ExecuteManyRequest {
        operation,
        parameter_sets,
        use_prepare,
        timeout,
        autocommit,
    } = request;
    let mut total = 0_i64;
    let mut has_known_count = false;
    let mut results = VecDeque::new();

    if parameter_sets.is_empty() && claim.drain_previous {
        client
            .close_query()
            .await
            .map_err(ExecuteFailure::broken)
            .map_err(|failure| ExecuteManyFailure {
                failure,
                row_index: 0,
            })?;
    }

    for (row_index, (rpc_parameters, parameter_signature)) in parameter_sets.into_iter().enumerate()
    {
        let outcome = execute_on_client(
            client,
            prepared_state,
            claim,
            ExecuteRequest {
                operation: operation.clone(),
                rpc_parameters,
                parameter_signature,
                use_prepare,
                reset_cursor: row_index == 0,
                timeout,
                autocommit,
                drain_previous: row_index == 0 && claim.drain_previous,
            },
        )
        .await
        .map_err(|failure| ExecuteManyFailure { failure, row_index })?;
        match outcome {
            ExecuteOutcome::Rows(metadata) => {
                results.push_back(
                    read_buffered_row_set(client, metadata)
                        .await
                        .map_err(|failure| ExecuteManyFailure { failure, row_index })?,
                );
            }
            ExecuteOutcome::NoRows(count) | ExecuteOutcome::TerminalNoRows(count) => {
                if count >= 0 {
                    has_known_count = true;
                    total = total.saturating_add(count);
                }
            }
        }

        while client.has_open_batch() {
            let next = client
                .advance()
                .await
                .map_err(ExecuteFailure::from)
                .map_err(|failure| ExecuteManyFailure { failure, row_index })?;
            match next {
                StatementResult::Rows => {
                    let metadata = client.get_metadata().clone();
                    results.push_back(
                        read_buffered_row_set(client, metadata)
                            .await
                            .map_err(|failure| ExecuteManyFailure { failure, row_index })?,
                    );
                }
                StatementResult::NoRows { rows_affected } => {
                    if let Some(count) = rows_affected {
                        has_known_count = true;
                        total = total.saturating_add(i64::try_from(count).unwrap_or(i64::MAX));
                    }
                }
                StatementResult::End => break,
            }
        }
        client.take_dml_result_counts();
        if (row_index + 1).is_multiple_of(EXECUTEMANY_EXECUTION_YIELD_INTERVAL) {
            tokio::task::yield_now().await;
        }
    }

    Ok((
        if results.is_empty() && has_known_count {
            total
        } else {
            -1
        },
        results,
    ))
}

async fn read_buffered_row_set(
    client: &mut TdsClient,
    metadata: Vec<ColumnMetadata>,
) -> Result<BufferedRowSet, ExecuteFailure> {
    let mut rows = VecDeque::new();
    loop {
        let mut writer = PyRowWriter::new(metadata.len());
        if !client.next_row_into(&mut writer).await? {
            break;
        }
        rows.push_back(writer);
        if rows.len().is_multiple_of(256) {
            tokio::task::yield_now().await;
        }
    }
    Ok(BufferedRowSet { metadata, rows })
}

async fn bind_parameter_sets(
    operation: String,
    seq_of_parameters: Py<PyAny>,
    hints: Option<Vec<ParameterHint>>,
) -> PyResult<(String, Vec<BoundParameterSet>)> {
    let iterator = Python::attach(|py| {
        seq_of_parameters
            .bind(py)
            .try_iter()
            .map(Bound::<PyIterator>::unbind)
    })?;
    let mut state = ExecuteManyBindingState {
        operation,
        iterator,
        hints,
        parameter_sets: Vec::new(),
        plan: None,
        named: None,
    };
    loop {
        let (next_state, exhausted) = tokio::task::spawn_blocking(move || {
            Python::attach(|py| bind_parameter_chunk(py, state))
        })
        .await
        .map_err(|error| {
            PyRuntimeError::new_err(format!(
                "ExecuteMany parameter binding task failed: {error}"
            ))
        })??;
        state = next_state;
        if exhausted {
            break;
        }
        tokio::task::yield_now().await;
    }
    let ExecuteManyBindingState {
        operation,
        parameter_sets,
        plan,
        ..
    } = state;
    Ok((
        plan.map_or_else(|| operation, |plan| plan.operation().to_string()),
        parameter_sets,
    ))
}

fn bind_parameter_chunk(
    py: Python<'_>,
    mut state: ExecuteManyBindingState,
) -> PyResult<(ExecuteManyBindingState, bool)> {
    let mut iterator = state.iterator.bind(py).clone();
    for _ in 0..EXECUTEMANY_PREFLIGHT_CHUNK_SIZE {
        let Some(row) = iterator.next() else {
            return Ok((state, true));
        };
        let row = row?;
        let row_index = state.parameter_sets.len();
        let row_is_named = row.cast::<PyDict>().is_ok();
        let row_is_positional = row.cast::<PyTuple>().is_ok()
            || row.cast::<PyList>().is_ok()
            || row.get_type().name()? == "Row";
        if !row_is_named && !row_is_positional {
            return Err(PyTypeError::new_err(format!(
                "executemany parameter row {row_index} must be a tuple, list, Row, or dict"
            )));
        }
        if state.named.is_some_and(|named| named != row_is_named) {
            return Err(PyTypeError::new_err(format!(
                "Mixed parameter types in executemany at row {row_index}"
            )));
        }
        state.named = Some(row_is_named);

        let plan = state
            .plan
            .get_or_insert(ParameterBindingPlan::new(&state.operation, row_is_named)?);
        let parameter_set = plan
            .bind_row(&row, state.hints.as_deref())
            .map_err(|error| {
                if error.is_instance_of::<PyTypeError>(py) {
                    PyTypeError::new_err(format!("executemany parameter row {row_index}: {error}"))
                } else {
                    error
                }
            })?;
        state.parameter_sets.push(parameter_set);
    }
    Ok((state, false))
}

fn map_execute_error(error: Error, info_messages: Vec<SqlInfoMessage>) -> PyErr {
    tracing::debug!("PyAsyncCursor::execute: failed: {error}");
    map_tds_error(
        "PyAsyncCursor.execute failed while executing query",
        error,
        info_messages,
    )
}

pub(crate) fn set_input_sizes(
    cursor: &mut PyAsyncCursor,
    sizes: &Bound<'_, PyAny>,
) -> PyResult<()> {
    cursor.replace_input_sizes(parse_input_sizes(sizes)?)
}

fn consume_input_sizes(cursor: &Py<PyAsyncCursor>, generation: u64) {
    Python::attach(|py| {
        let mut cursor = cursor.borrow_mut(py);
        if cursor.input_sizes_generation() == generation {
            cursor.clear_input_sizes();
        }
    });
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
        description_state,
        rowcount,
        buffered_results,
    } = resources;
    let (operation, rpc_parameters, parameter_signature) =
        bind_parameters(operation, parameters, input_sizes.as_deref())?;
    let claim = session_state
        .claim_execute(cursor_id)
        .map_err(map_claim_error)?;
    let request = ExecuteRequest {
        operation,
        rpc_parameters,
        parameter_signature,
        use_prepare,
        reset_cursor,
        timeout,
        autocommit,
        drain_previous: claim.drain_previous,
    };
    let operation_id = claim.operation_id;
    let future_state = session_state.clone();
    let previous_fetch_status = fetch_state.replace(FetchStatus::NoResultSet);
    let future_fetch_state = fetch_state.clone();
    let previous_description = description_state.replace(None);
    let future_description_state = description_state.clone();
    let previous_rowcount = rowcount.swap(-1, Ordering::AcqRel);
    let previous_buffered_results = buffered_results.replace(VecDeque::new());
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

        let (result, info_messages, has_open_batch) = {
            let mut client = client.lock().await;
            let result = execute_on_client(&mut client, &prepared_state, &claim, request).await;
            let info_messages = if matches!(
                result,
                Err(ExecuteFailure {
                    error: Error::SqlServerError { .. },
                    ..
                })
            ) {
                client.take_info_messages()
            } else {
                Vec::new()
            };
            let has_open_batch = client.has_open_batch();
            (result, info_messages, has_open_batch)
        };

        match result {
            Ok(outcome) => {
                future_rowcount.store(outcome.rowcount(), Ordering::Release);
                let has_open_batch = outcome.has_open_batch();
                let has_result_set = outcome.has_rows();
                let fetch_status = outcome.fetch_status();
                record_result_set_status(outcome.result_set_status());
                operation_guard.finish_execute(has_open_batch);
                let metadata = match outcome {
                    ExecuteOutcome::Rows(metadata) => Some(metadata),
                    ExecuteOutcome::NoRows(_) | ExecuteOutcome::TerminalNoRows(_) => None,
                };
                let column_count = metadata.as_ref().map_or(0, Vec::len);
                consume_input_sizes(&cursor, input_sizes_generation);
                let description_started = Instant::now();
                let description = materialize(metadata).await.map_err(|error| {
                    tracing::error!(
                        "PyAsyncCursor::execute: cursor description materialization failed; column_count={column_count}; elapsed_ms={}; error={error}",
                        description_started.elapsed().as_millis()
                    );
                    InternalError::new_err(format!(
                        "Query executed but cursor description materialization failed: {error}"
                    ))
                })?;
                let description_materialization_ms = description_started.elapsed().as_millis();
                future_description_state.replace(description);
                future_fetch_state.set(fetch_status);
                tracing::info!(
                    "PyAsyncCursor::execute: query executed successfully; has_result_set={has_result_set}; column_count={column_count}; description_materialization_ms={description_materialization_ms}; has_open_batch={has_open_batch}"
                );
                Ok(cursor)
            }
            Err(error) => {
                record_result_set_status("error");
                operation_guard.settle(error.break_connection || has_open_batch);
                Err(map_execute_error(error.error, info_messages))
            }
        }
    };
    let future = in_cursor_operation_span(future, cursor_id, operation_id, "execute", "pending");
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
            fetch_state.set(previous_fetch_status);
            description_state.replace(previous_description);
            rowcount.store(previous_rowcount, Ordering::Release);
            buffered_results.replace(previous_buffered_results);
            Err(error)
        }
    }
}

pub(crate) fn executemany<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
    operation: String,
    seq_of_parameters: &Bound<'_, PyAny>,
    use_prepare: bool,
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
        description_state,
        rowcount,
        buffered_results,
    } = resources;
    let seq_of_parameters = seq_of_parameters.clone().unbind();
    let trace_dispatch = dispatch.clone();

    let future = async move {
        let started = Instant::now();
        let preflight_started = Instant::now();
        let mut preflight_guard = ExecuteManyPreflightGuard::new(cursor_id, trace_dispatch.clone());
        tracing::debug!(
            cursor_id,
            "PyAsyncCursor::executemany: parameter preflight started; cursor_id={cursor_id}"
        );
        let preflight = bind_parameter_sets(operation, seq_of_parameters, input_sizes).await;
        let preflight_ms = preflight_started.elapsed().as_millis();
        let (operation, parameter_sets) = match preflight {
            Ok(bound) => {
                preflight_guard.complete();
                bound
            }
            Err(error) => {
                preflight_guard.complete();
                tracing::error!(
                    cursor_id,
                    preflight_ms,
                    error = %error,
                    "PyAsyncCursor::executemany: parameter preflight failed; cursor_id={cursor_id}; preflight_ms={preflight_ms}; error={error}"
                );
                return Err(error);
            }
        };
        let parameter_count = parameter_sets.first().map_or(0, |set| set.0.len());
        let batch_count = parameter_sets.len();
        tracing::debug!(
            cursor_id,
            batch_count,
            parameter_count,
            preflight_ms,
            "PyAsyncCursor::executemany: parameter preflight completed; cursor_id={cursor_id}; batch_count={batch_count}; parameter_count={parameter_count}; preflight_ms={preflight_ms}"
        );
        let request = ExecuteManyRequest {
            operation,
            parameter_sets,
            use_prepare,
            timeout,
            autocommit,
        };
        let claim = session_state
            .claim_execute(cursor_id)
            .map_err(map_claim_error)?;
        let operation_id = claim.operation_id;
        let execution = async move {
            fetch_state.set(FetchStatus::NoResultSet);
            description_state.replace(None);
            rowcount.store(-1, Ordering::Release);
            buffered_results.replace(VecDeque::new());
            let mut operation_guard =
                SessionOperationGuard::new(session_state.clone(), operation_id);
            let mut interruption_guard =
                ExecuteManyInterruptionGuard::new(cursor_id, operation_id, trace_dispatch);
            cleanup_required.store(true, Ordering::Release);
            tracing::info!(
                batch_count,
                parameter_count,
                use_prepare,
                preflight_ms,
                "PyAsyncCursor::executemany: executing parameter rows; batch_count={batch_count}; parameter_count={parameter_count}; use_prepare={use_prepare}; preflight_ms={preflight_ms}"
            );

            let execution_started = Instant::now();
            let (result, info_messages, has_open_batch) = {
                let mut client = client.lock().await;
                let result =
                    execute_many_on_client(&mut client, &prepared_state, &claim, request).await;
                let info_messages = if result.is_err() {
                    client.take_info_messages()
                } else {
                    Vec::new()
                };
                let has_open_batch = client.has_open_batch();
                (result, info_messages, has_open_batch)
            };
            let execution_ms = execution_started.elapsed().as_millis();

            match result {
                Ok((total_rows_affected, results)) => {
                    let produced_rows = !results.is_empty();
                    let result_set_count = results.len();
                    let buffered_row_count = results
                        .iter()
                        .map(|result| result.rows.len())
                        .sum::<usize>();
                    let total_rows_affected = if batch_count == 0 {
                        0
                    } else {
                        total_rows_affected
                    };
                    let metadata = results.front().map(|result| result.metadata.clone());
                    interruption_guard.set_phase("description_materialization");
                    let description_started = Instant::now();
                    let description = match materialize(metadata).await {
                        Ok(description) => description,
                        Err(error) => {
                            let description_materialization_ms =
                                description_started.elapsed().as_millis();
                            record_result_set_status("error");
                            tracing::error!(
                                batch_count,
                                result_set_count,
                                buffered_row_count,
                                preflight_ms,
                                execution_ms,
                                description_materialization_ms,
                                error = %error,
                                "PyAsyncCursor::executemany: cursor description materialization failed; batch_count={batch_count}; result_set_count={result_set_count}; buffered_row_count={buffered_row_count}; preflight_ms={preflight_ms}; execution_ms={execution_ms}; description_materialization_ms={description_materialization_ms}; error={error}"
                            );
                            operation_guard.settle(false);
                            interruption_guard.complete();
                            return Err(InternalError::new_err(format!(
                                "ExecuteMany completed but cursor description materialization failed: {error}"
                            )));
                        }
                    };
                    let description_materialization_ms = description_started.elapsed().as_millis();
                    buffered_results.replace(results);
                    description_state.replace(description);
                    fetch_state.set(if produced_rows {
                        FetchStatus::Ready
                    } else {
                        FetchStatus::NoResultSet
                    });
                    rowcount.store(total_rows_affected, Ordering::Release);
                    operation_guard.finish_execute(produced_rows);
                    interruption_guard.complete();
                    if batch_count > 0 {
                        consume_input_sizes(&cursor, input_sizes_generation);
                    }
                    record_result_set_status(if produced_rows {
                        "rows_drained"
                    } else {
                        "no_rows"
                    });
                    let elapsed_ms = started.elapsed().as_millis();
                    tracing::info!(
                        batch_count,
                        total_rows_affected,
                        produced_rows,
                        result_set_count,
                        buffered_row_count,
                        preflight_ms,
                        execution_ms,
                        description_materialization_ms,
                        elapsed_ms,
                        "PyAsyncCursor::executemany: completed; batch_count={batch_count}; total_rows_affected={total_rows_affected}; produced_rows={produced_rows}; result_set_count={result_set_count}; buffered_row_count={buffered_row_count}; preflight_ms={preflight_ms}; execution_ms={execution_ms}; description_materialization_ms={description_materialization_ms}; elapsed_ms={elapsed_ms}"
                    );
                    Ok(cursor)
                }
                Err(error) => {
                    record_result_set_status("error");
                    let connection_marked_broken = error.failure.break_connection || has_open_batch;
                    operation_guard.settle(connection_marked_broken);
                    interruption_guard.complete();
                    let elapsed_ms = started.elapsed().as_millis();
                    tracing::error!(
                        failed_row_index = error.row_index,
                        connection_marked_broken,
                        preflight_ms,
                        execution_ms,
                        elapsed_ms,
                        error = %error.failure.error,
                        "PyAsyncCursor::executemany: failed; failed_row_index={}; connection_marked_broken={connection_marked_broken}; preflight_ms={preflight_ms}; execution_ms={execution_ms}; elapsed_ms={elapsed_ms}; error={}",
                        error.row_index,
                        error.failure.error
                    );
                    let error = map_tds_error(
                        &format!(
                            "PyAsyncCursor.executemany failed while executing parameter row {}",
                            error.row_index
                        ),
                        error.failure.error,
                        info_messages,
                    );
                    fetch_state.set(FetchStatus::NoResultSet);
                    description_state.replace(None);
                    rowcount.store(-1, Ordering::Release);
                    buffered_results.replace(VecDeque::new());
                    Err(error)
                }
            }
        };
        in_cursor_operation_span(execution, cursor_id, operation_id, "executemany", "pending").await
    };
    let future = async move {
        match dispatch {
            Some(dispatch) => future.with_subscriber(dispatch).await,
            None => future.await,
        }
    };

    pyo3_async_runtimes::tokio::future_into_py(py, future)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mssql_tds::connection::tds_client::PreparedStatement;
    use mssql_tds::error::Error;

    use super::{
        ExecuteFailure, ParameterMetadata, PreparedState, should_replace_prepared_statement,
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

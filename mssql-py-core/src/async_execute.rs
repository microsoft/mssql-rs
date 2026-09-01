// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous command execution for [`crate::async_cursor::PyAsyncCursor`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use mssql_tds::connection::tds_client::{
    ExecuteOptions, PreparedStatement, ResultSet, StatementId, StatementResult, TdsClient,
};
use mssql_tds::error::Error;
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use mssql_tds::query::metadata::ColumnMetadata;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use crate::async_cursor::{PyAsyncCursor, map_claim_error};
use crate::async_description::{DescriptionState, materialize};
use crate::async_fetch::{FetchState, FetchStatus};
use crate::async_parameters::{ParameterMetadata, bind_parameters, parse_input_sizes};
use crate::async_session::{AsyncConnectionState, CursorId, ExecuteClaim, SessionOperationGuard};
use crate::async_tracing::{in_cursor_operation_span, record_result_set_status};
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
    description_state: Arc<DescriptionState>,
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
        }
    }
}

enum ExecuteOutcome {
    Idle,
    NoRows,
    Rows(Vec<ColumnMetadata>),
}

impl ExecuteOutcome {
    fn has_open_batch(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    fn has_rows(&self) -> bool {
        matches!(self, Self::Rows(_))
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
    Ok(match first {
        StatementResult::Rows => ExecuteOutcome::Rows(client.get_metadata().clone()),
        StatementResult::NoRows { .. } if client.has_open_batch() => ExecuteOutcome::NoRows,
        StatementResult::NoRows { .. } | StatementResult::End => ExecuteOutcome::Idle,
    })
}

fn map_execute_error(error: impl std::fmt::Display) -> PyErr {
    tracing::debug!("PyAsyncCursor::execute: failed: {error}");
    PyRuntimeError::new_err(format!("Query execution failed: {error}"))
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
        description_state,
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
    let previous_description = description_state.replace(None);
    let future_description_state = description_state.clone();

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
            let result = execute_on_client(&mut client, &prepared_state, &claim, request).await;
            let has_open_batch = client.has_open_batch();
            (result, has_open_batch)
        };

        match result {
            Ok(outcome) => {
                let has_open_batch = outcome.has_open_batch();
                let has_result_set = outcome.has_rows();
                record_result_set_status(if has_result_set {
                    "rows"
                } else if has_open_batch {
                    "no_rows"
                } else {
                    "exhausted"
                });
                operation_guard.finish_execute(has_open_batch);
                let metadata = match outcome {
                    ExecuteOutcome::Rows(metadata) => Some(metadata),
                    ExecuteOutcome::Idle | ExecuteOutcome::NoRows => None,
                };
                let column_count = metadata.as_ref().map_or(0, Vec::len);
                future_fetch_state.set(if has_result_set {
                    FetchStatus::Ready
                } else {
                    FetchStatus::NoResultSet
                });
                Python::attach(|py| {
                    let mut cursor_ref = cursor.borrow_mut(py);
                    if cursor_ref.input_sizes_generation() == input_sizes_generation {
                        cursor_ref.clear_input_sizes();
                    }
                });
                let description_started = Instant::now();
                let description = materialize(metadata).await.map_err(|error| {
                    tracing::error!(
                        "PyAsyncCursor::execute: cursor description materialization failed; column_count={column_count}; elapsed_ms={}; error={error}",
                        description_started.elapsed().as_millis()
                    );
                    PyRuntimeError::new_err(format!(
                        "Query executed but cursor description materialization failed: {error}"
                    ))
                })?;
                let description_materialization_ms = description_started.elapsed().as_millis();
                future_description_state.replace(description);
                tracing::info!(
                    "PyAsyncCursor::execute: query executed successfully; has_result_set={has_result_set}; column_count={column_count}; description_materialization_ms={description_materialization_ms}; has_open_batch={has_open_batch}"
                );
                Ok(cursor)
            }
            Err(error) => {
                record_result_set_status("error");
                operation_guard.settle(error.break_connection || has_open_batch);
                Err(map_execute_error(error.error))
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
            Err(error)
        }
    }
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

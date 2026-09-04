// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous row fetching for [`crate::async_cursor::PyAsyncCursor`].

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use mssql_tds::connection::tds_client::StatementResult;
use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::error::{Error, SqlInfoMessage};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyList;
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use crate::async_cursor::{PyAsyncCursor, map_claim_error};
use crate::async_description::{DescriptionState, materialize};
use crate::async_errors::{InternalError, map_tds_error};
use crate::async_session::{AsyncConnectionState, ClaimError, CursorId, OperationId};
use crate::async_tracing::{in_cursor_operation_span, record_result_set_status};
use crate::row_writer::PyRowWriter;

const FETCH_YIELD_INTERVAL: usize = 256;
const LIST_MATERIALIZE_CHUNK_SIZE: usize = 256;
const ATTENTION_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum FetchStatus {
    NoResultSet,
    Ready,
    Exhausted,
    TerminalNoRows,
}

pub(crate) struct FetchState(AtomicU8);

impl FetchState {
    pub(crate) fn new() -> Self {
        Self(AtomicU8::new(FetchStatus::NoResultSet as u8))
    }

    pub(crate) fn status(&self) -> FetchStatus {
        match self.0.load(Ordering::Acquire) {
            value if value == FetchStatus::Ready as u8 => FetchStatus::Ready,
            value if value == FetchStatus::Exhausted as u8 => FetchStatus::Exhausted,
            value if value == FetchStatus::TerminalNoRows as u8 => FetchStatus::TerminalNoRows,
            _ => FetchStatus::NoResultSet,
        }
    }

    pub(crate) fn replace(&self, status: FetchStatus) -> FetchStatus {
        let previous = self.0.swap(status as u8, Ordering::AcqRel);
        match previous {
            value if value == FetchStatus::Ready as u8 => FetchStatus::Ready,
            value if value == FetchStatus::Exhausted as u8 => FetchStatus::Exhausted,
            value if value == FetchStatus::TerminalNoRows as u8 => FetchStatus::TerminalNoRows,
            _ => FetchStatus::NoResultSet,
        }
    }

    pub(crate) fn set(&self, status: FetchStatus) {
        self.0.store(status as u8, Ordering::Release);
    }
}

/// Python-independent resources captured before constructing a fetch future.
pub(crate) struct FetchResources {
    client: Arc<Mutex<TdsClient>>,
    dispatch: Option<tracing::Dispatch>,
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    fetch_state: Arc<FetchState>,
    description_state: Arc<DescriptionState>,
}

impl FetchResources {
    pub(crate) fn new(
        client: Arc<Mutex<TdsClient>>,
        dispatch: Option<tracing::Dispatch>,
        session_state: Arc<AsyncConnectionState>,
        cursor_id: CursorId,
        fetch_state: Arc<FetchState>,
        description_state: Arc<DescriptionState>,
    ) -> Self {
        Self {
            client,
            dispatch,
            session_state,
            cursor_id,
            fetch_state,
            description_state,
        }
    }
}

/// Settles session ownership when a fetch task completes or fails.
struct FetchGuard {
    session_state: Arc<AsyncConnectionState>,
    operation_id: OperationId,
    operation: &'static str,
    dispatch: Option<tracing::Dispatch>,
    completed: bool,
}

impl FetchGuard {
    fn new(
        session_state: Arc<AsyncConnectionState>,
        operation_id: OperationId,
        operation: &'static str,
        dispatch: Option<tracing::Dispatch>,
    ) -> Self {
        Self {
            session_state,
            operation_id,
            operation,
            dispatch,
            completed: false,
        }
    }

    /// Publishes the fetch result only if cancellation has not already won.
    ///
    /// The return value lets the detached worker suppress stale rows and settle
    /// ATTENTION when Python cancels while protocol work is completing.
    fn complete(&mut self, result_set_exhausted: bool, has_open_batch: bool) -> bool {
        self.completed = self.session_state.finish_fetch(
            self.operation_id,
            result_set_exhausted,
            has_open_batch,
        );
        self.completed
    }

    /// Releases session ownership after a failed fetch.
    ///
    /// The connection is marked broken only when the protocol task could not
    /// prove that it reached a reusable TDS boundary.
    fn fail(&mut self, break_connection: bool) {
        if break_connection {
            self.session_state.mark_broken();
        }
        self.session_state.release_operation(self.operation_id);
        self.completed = true;
    }
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        if !self.completed {
            let _guard = self.dispatch.as_ref().map(tracing::dispatcher::set_default);
            tracing::warn!(
                "PyAsyncCursor::{}: interrupted; connection marked broken",
                self.operation
            );
            self.session_state.mark_broken();
            self.session_state.release_operation(self.operation_id);
        }
    }
}

/// Converts cancellation of a Python awaitable into protocol cancellation.
///
/// The TDS work runs in a detached Tokio task so dropping the Python-facing
/// future cannot abandon a parser mid-token. This guard signals that task to
/// send ATTENTION while leaving it responsible for settling the session.
struct FetchCancellationGuard {
    session_state: Arc<AsyncConnectionState>,
    operation_id: OperationId,
    operation: &'static str,
    dispatch: Option<tracing::Dispatch>,
    completed: bool,
}

impl FetchCancellationGuard {
    /// Arms cancellation tracking around the join of a detached fetch task.
    fn new(
        session_state: Arc<AsyncConnectionState>,
        operation_id: OperationId,
        operation: &'static str,
        dispatch: Option<tracing::Dispatch>,
    ) -> Self {
        Self {
            session_state,
            operation_id,
            operation,
            dispatch,
            completed: false,
        }
    }

    /// Disarms cancellation after the detached task has finished settling TDS.
    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for FetchCancellationGuard {
    /// Requests ATTENTION when Python stops waiting before settlement finishes.
    fn drop(&mut self) {
        if !self.completed && self.session_state.cancel_fetch(self.operation_id) {
            let _guard = self.dispatch.as_ref().map(tracing::dispatcher::set_default);
            tracing::debug!(
                "PyAsyncCursor::{}: cancelled; ATTENTION settlement continues in the background",
                self.operation
            );
        }
    }
}

struct MaterializationGuard {
    operation: &'static str,
    dispatch: Option<tracing::Dispatch>,
    completed: bool,
}

impl MaterializationGuard {
    fn new(operation: &'static str, dispatch: Option<tracing::Dispatch>) -> Self {
        Self {
            operation,
            dispatch,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for MaterializationGuard {
    fn drop(&mut self) {
        if !self.completed {
            let _guard = self.dispatch.as_ref().map(tracing::dispatcher::set_default);
            tracing::warn!(
                "PyAsyncCursor::{}: interrupted during row materialization; connection remains usable",
                self.operation
            );
        }
    }
}

struct FetchBatch {
    rows: Vec<PyRowWriter>,
    exhausted: bool,
}

#[derive(Clone, Copy)]
enum FetchOutput {
    One,
    Many,
    All,
}

impl FetchOutput {
    fn empty(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self {
            Self::One => Ok(py.None()),
            Self::Many | Self::All => Ok(PyList::empty(py).into_any().unbind()),
        }
    }
}

async fn materialize_rows(output: FetchOutput, rows: Vec<PyRowWriter>) -> PyResult<Py<PyAny>> {
    match output {
        FetchOutput::One => tokio::task::spawn_blocking(move || {
            Python::attach(|py| match rows.into_iter().next() {
                Some(writer) => Ok(writer.to_py_tuple(py)?.into_any().unbind()),
                None => Ok(py.None()),
            })
        })
        .await
        .map_err(map_materialization_join_error)?,
        FetchOutput::Many | FetchOutput::All => materialize_list_rows(rows).await,
    }
}

async fn materialize_list_rows(rows: Vec<PyRowWriter>) -> PyResult<Py<PyAny>> {
    let mut rows = rows.into_iter();
    let mut list: Option<Py<PyList>> = None;

    loop {
        let chunk = rows
            .by_ref()
            .take(LIST_MATERIALIZE_CHUNK_SIZE)
            .collect::<Vec<_>>();
        let finished = chunk.len() < LIST_MATERIALIZE_CHUNK_SIZE;
        list = Some(
            tokio::task::spawn_blocking(move || {
                Python::attach(|py| {
                    let converted = chunk
                        .into_iter()
                        .map(|writer| writer.to_py_tuple(py).map(Bound::unbind))
                        .collect::<PyResult<Vec<_>>>()?;
                    let chunk = PyList::new(py, converted)?;
                    let list = match list {
                        Some(list) => {
                            let bound = list.bind(py);
                            let end = bound.len();
                            bound.set_slice(end, end, chunk.as_any())?;
                            list
                        }
                        None => chunk.unbind(),
                    };
                    Ok::<Py<PyList>, PyErr>(list)
                })
            })
            .await
            .map_err(map_materialization_join_error)??,
        );
        if finished {
            return Ok(list
                .expect("materialization always creates a list")
                .into_any());
        }
        tokio::task::yield_now().await;
    }
}

fn map_materialization_join_error(error: tokio::task::JoinError) -> PyErr {
    PyRuntimeError::new_err(format!("Failed to materialize fetched rows: {error}"))
}

/// Runs fetch protocol work in a task that outlives its Python awaitable.
///
/// Tokio detaches a spawned task when its join handle is dropped. Keeping the
/// TDS future inside that task preserves parser state long enough to drain
/// through DONE_ATTN; the surrounding cancellation guard only signals it.
async fn run_fetch_in_background<F, T>(
    future: F,
    session_state: Arc<AsyncConnectionState>,
    operation_id: OperationId,
    operation: &'static str,
    dispatch: Option<tracing::Dispatch>,
) -> PyResult<T>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: Send + 'static,
{
    let mut cancellation_guard =
        FetchCancellationGuard::new(session_state, operation_id, operation, dispatch);
    let result = tokio::spawn(future).await;
    cancellation_guard.complete();
    result.map_err(|error| {
        PyRuntimeError::new_err(format!(
            "PyAsyncCursor.{operation} background task failed: {error}"
        ))
    })?
}

async fn fetch_rows_on_client(client: &mut TdsClient, limit: usize) -> Result<FetchBatch, Error> {
    if !client.on_rows() {
        return Ok(FetchBatch {
            rows: Vec::new(),
            exhausted: true,
        });
    }

    let column_count = client.get_metadata().len();
    let mut rows = Vec::with_capacity(limit.min(1024));
    while rows.len() < limit {
        let mut writer = PyRowWriter::new(column_count);
        if !client.next_row_into(&mut writer).await? {
            return Ok(FetchBatch {
                rows,
                exhausted: true,
            });
        }
        rows.push(writer);
        if rows.len() % FETCH_YIELD_INTERVAL == 0 {
            tokio::task::yield_now().await;
        }
    }
    Ok(FetchBatch {
        rows,
        exhausted: false,
    })
}

fn map_fetch_error(
    operation: &str,
    error: Error,
    info_messages: Vec<SqlInfoMessage>,
    elapsed_ms: u128,
) -> PyErr {
    tracing::error!("PyAsyncCursor::{operation}: failed; elapsed_ms={elapsed_ms}; error={error}");
    map_tds_error(
        &format!("PyAsyncCursor.{operation} failed while reading rows"),
        error,
        info_messages,
    )
}

fn map_nextset_error(error: Error, info_messages: Vec<SqlInfoMessage>, elapsed_ms: u128) -> PyErr {
    tracing::error!("PyAsyncCursor::nextset: failed; elapsed_ms={elapsed_ms}; error={error}");
    map_tds_error(
        "PyAsyncCursor.nextset failed while advancing results",
        error,
        info_messages,
    )
}

fn fetch<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
    resources: FetchResources,
    limit: usize,
    output: FetchOutput,
    operation: &'static str,
) -> PyResult<Bound<'py, PyAny>> {
    let FetchResources {
        client,
        dispatch,
        session_state,
        cursor_id,
        fetch_state,
        description_state: _,
    } = resources;
    if matches!(
        fetch_state.status(),
        FetchStatus::NoResultSet | FetchStatus::TerminalNoRows
    ) {
        session_state.ensure_open().map_err(map_claim_error)?;
        return Err(map_claim_error(ClaimError::NoResultSet));
    }
    if fetch_state.status() == FetchStatus::Exhausted {
        session_state.ensure_open().map_err(map_claim_error)?;
        return pyo3_async_runtimes::tokio::future_into_py(py, async move {
            Python::attach(|py| output.empty(py))
        });
    }
    let claim = session_state
        .claim_fetch(cursor_id)
        .map_err(map_claim_error)?;
    let operation_id = claim.operation_id;
    let future_state = session_state.clone();
    let cancellation_state = session_state.clone();
    let guard_dispatch = dispatch.clone();
    let cancellation_dispatch = dispatch.clone();
    let materialization_dispatch = dispatch.clone();

    let future = async move {
        let started = Instant::now();
        match output {
            FetchOutput::Many => {
                tracing::debug!("PyAsyncCursor::{operation}: started; requested={limit}");
            }
            FetchOutput::All => tracing::debug!("PyAsyncCursor::{operation}: started"),
            FetchOutput::One => {}
        }
        // Retain the Python cursor until the row operation settles so its finalizer
        // cannot race the in-flight TDS read.
        let _cursor = cursor;
        let mut fetch_guard =
            FetchGuard::new(future_state, operation_id, operation, guard_dispatch);

        let (result, info_messages, has_open_batch, connection_dead) = {
            let mut client = client.lock().await;
            let result = fetch_rows_on_client(&mut client, limit).await;
            let has_open_batch = client.has_open_batch();
            let info_messages = if matches!(result, Err(Error::SqlServerError { .. })) {
                client.take_info_messages()
            } else {
                Vec::new()
            };
            let connection_dead = client.is_connection_dead();
            (result, info_messages, has_open_batch, connection_dead)
        };
        let read_ms = started.elapsed().as_millis();

        match result {
            Ok(batch) => {
                let returned = batch.rows.len();
                let exhausted = batch.exhausted;
                record_result_set_status(if exhausted { "exhausted" } else { "ready" });
                let fetch_status = if exhausted {
                    FetchStatus::Exhausted
                } else {
                    FetchStatus::Ready
                };
                if !fetch_guard.complete(batch.exhausted, has_open_batch) {
                    let (has_open_batch, connection_dead) = {
                        let mut client = client.lock().await;
                        if client.has_open_batch() {
                            let _ = client
                                .send_attention_with_timeout(ATTENTION_SETTLEMENT_TIMEOUT)
                                .await;
                        }
                        (client.has_open_batch(), client.is_connection_dead())
                    };
                    fetch_guard.fail(has_open_batch || connection_dead);
                    fetch_state.set(FetchStatus::NoResultSet);
                    return Err(map_fetch_error(
                        operation,
                        Error::OperationCancelledError("Fetch was cancelled".to_string()),
                        Vec::new(),
                        started.elapsed().as_millis(),
                    ));
                }
                fetch_state.set(fetch_status);
                let materialization_started = Instant::now();
                let mut materialization_guard =
                    MaterializationGuard::new(operation, materialization_dispatch);
                match materialize_rows(output, batch.rows).await {
                    Ok(rows) => {
                        materialization_guard.complete();
                        let materialization_ms = materialization_started.elapsed().as_millis();
                        match output {
                            FetchOutput::Many => tracing::debug!(
                                "PyAsyncCursor::{operation}: completed; requested={limit}; returned={returned}; exhausted={exhausted}; elapsed_ms={}; read_ms={read_ms}; materialization_ms={materialization_ms}",
                                started.elapsed().as_millis(),
                            ),
                            FetchOutput::All => tracing::debug!(
                                "PyAsyncCursor::{operation}: completed; returned={returned}; exhausted={exhausted}; elapsed_ms={}; read_ms={read_ms}; materialization_ms={materialization_ms}",
                                started.elapsed().as_millis(),
                            ),
                            FetchOutput::One => {}
                        }
                        if exhausted {
                            tracing::info!("PyAsyncCursor::{operation}: result set exhausted");
                        }
                        Ok(rows)
                    }
                    Err(error) => {
                        materialization_guard.complete();
                        tracing::error!(
                            "PyAsyncCursor::{operation}: row materialization failed; returned={returned}; elapsed_ms={}; read_ms={read_ms}; materialization_ms={}; error={error}",
                            started.elapsed().as_millis(),
                            materialization_started.elapsed().as_millis(),
                        );
                        Err(error)
                    }
                }
            }
            Err(error) => {
                record_result_set_status("error");
                fetch_guard.fail(has_open_batch || connection_dead);
                fetch_state.set(FetchStatus::NoResultSet);
                Err(map_fetch_error(
                    operation,
                    error,
                    info_messages,
                    started.elapsed().as_millis(),
                ))
            }
        }
    };
    let future = in_cursor_operation_span(future, cursor_id, operation_id, operation, "reading");
    let future = async move {
        match dispatch {
            Some(dispatch) => future.with_subscriber(dispatch).await,
            None => future.await,
        }
    };
    let future = run_fetch_in_background(
        future,
        cancellation_state,
        operation_id,
        operation,
        cancellation_dispatch,
    );

    match pyo3_async_runtimes::tokio::future_into_py(py, future) {
        Ok(awaitable) => Ok(awaitable),
        Err(error) => {
            session_state.restore_fetch(operation_id);
            Err(error)
        }
    }
}

/// Return an awaitable resolving to the next row tuple or `None`.
pub(crate) fn fetchone<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
) -> PyResult<Bound<'py, PyAny>> {
    let resources = cursor.borrow(py).fetch_resources()?;
    fetch(cursor, py, resources, 1, FetchOutput::One, "fetchone")
}

/// Return an awaitable resolving to at most `size` row tuples.
pub(crate) fn fetchmany<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
    size: isize,
) -> PyResult<Bound<'py, PyAny>> {
    let resources = cursor.borrow(py).fetch_resources()?;
    if size <= 0 {
        resources
            .session_state
            .ensure_open()
            .map_err(map_claim_error)?;
        if matches!(
            resources.fetch_state.status(),
            FetchStatus::NoResultSet | FetchStatus::TerminalNoRows
        ) {
            return Err(map_claim_error(ClaimError::NoResultSet));
        }
        return pyo3_async_runtimes::tokio::future_into_py(py, async move {
            Python::attach(|py| FetchOutput::Many.empty(py))
        });
    }
    fetch(
        cursor,
        py,
        resources,
        size as usize,
        FetchOutput::Many,
        "fetchmany",
    )
}

/// Return an awaitable resolving to all remaining rows in the current result set.
pub(crate) fn fetchall<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
) -> PyResult<Bound<'py, PyAny>> {
    let resources = cursor.borrow(py).fetch_resources()?;
    fetch(
        cursor,
        py,
        resources,
        usize::MAX,
        FetchOutput::All,
        "fetchall",
    )
}

/// Return an awaitable that advances to the next statement result.
pub(crate) fn nextset<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
) -> PyResult<Bound<'py, PyAny>> {
    let resources = cursor.borrow(py).fetch_resources()?;
    let FetchResources {
        client,
        dispatch,
        session_state,
        cursor_id,
        fetch_state,
        description_state,
    } = resources;
    let claim = match session_state.claim_fetch(cursor_id) {
        Ok(claim) => claim,
        Err(ClaimError::NoResultSet)
            if matches!(
                fetch_state.status(),
                FetchStatus::Exhausted | FetchStatus::TerminalNoRows
            ) =>
        {
            return pyo3_async_runtimes::tokio::future_into_py(py, async { Ok(false) });
        }
        Err(error) => return Err(map_claim_error(error)),
    };
    let operation_id = claim.operation_id;
    let future_state = session_state.clone();
    let cancellation_state = session_state.clone();
    let previous_fetch_status = fetch_state.replace(FetchStatus::NoResultSet);
    let previous_description = description_state.replace(None);
    let future_fetch_state = fetch_state.clone();
    let future_description_state = description_state.clone();
    let guard_dispatch = dispatch.clone();
    let cancellation_dispatch = dispatch.clone();

    let future = async move {
        let started = Instant::now();
        tracing::debug!("PyAsyncCursor::nextset: started");
        let _cursor = cursor;
        let mut fetch_guard =
            FetchGuard::new(future_state, operation_id, "nextset", guard_dispatch);

        let (result, info_messages, has_open_batch, connection_dead) = {
            let mut client = client.lock().await;
            let result = client.advance().await.map(|result| {
                let metadata =
                    matches!(result, StatementResult::Rows).then(|| client.get_metadata().clone());
                (result, metadata)
            });
            let has_open_batch = client.has_open_batch();
            let info_messages = if matches!(result, Err(Error::SqlServerError { .. })) {
                client.take_info_messages()
            } else {
                Vec::new()
            };
            (
                result,
                info_messages,
                has_open_batch,
                client.is_connection_dead(),
            )
        };
        let read_ms = started.elapsed().as_millis();

        match result {
            Ok((result, metadata)) => {
                let has_result = !matches!(result, StatementResult::End);
                let has_rows = matches!(result, StatementResult::Rows);
                record_result_set_status(match result {
                    StatementResult::Rows => "rows",
                    StatementResult::NoRows { .. } => "no_rows",
                    StatementResult::End => "exhausted",
                });
                let column_count = metadata.as_ref().map_or(0, Vec::len);
                let next_fetch_status = match result {
                    StatementResult::Rows => FetchStatus::Ready,
                    StatementResult::NoRows { .. } if has_open_batch => FetchStatus::NoResultSet,
                    StatementResult::NoRows { .. } => FetchStatus::TerminalNoRows,
                    StatementResult::End => FetchStatus::Exhausted,
                };

                let materialization_started = Instant::now();
                let materialized = materialize(metadata).await;
                if !fetch_guard.complete(!has_rows, has_open_batch) {
                    let (has_open_batch, connection_dead) = {
                        let mut client = client.lock().await;
                        if client.has_open_batch() {
                            let _ = client
                                .send_attention_with_timeout(ATTENTION_SETTLEMENT_TIMEOUT)
                                .await;
                        }
                        (client.has_open_batch(), client.is_connection_dead())
                    };
                    fetch_guard.fail(has_open_batch || connection_dead);
                    future_fetch_state.set(FetchStatus::NoResultSet);
                    future_description_state.replace(None);
                    return Err(map_nextset_error(
                        Error::OperationCancelledError(
                            "Result-set advance was cancelled".to_string(),
                        ),
                        Vec::new(),
                        started.elapsed().as_millis(),
                    ));
                }

                match materialized {
                    Ok(description) => {
                        future_description_state.replace(description);
                        future_fetch_state.set(next_fetch_status);
                        tracing::debug!(
                            "PyAsyncCursor::nextset: completed; has_result={has_result}; has_rows={has_rows}; column_count={column_count}; elapsed_ms={}; read_ms={read_ms}; materialization_ms={}",
                            started.elapsed().as_millis(),
                            materialization_started.elapsed().as_millis(),
                        );
                        if !has_result {
                            tracing::info!("PyAsyncCursor::nextset: batch exhausted");
                        }
                        Ok(has_result)
                    }
                    Err(error) => {
                        future_fetch_state.set(FetchStatus::NoResultSet);
                        tracing::error!(
                            "PyAsyncCursor::nextset: description materialization failed; column_count={column_count}; elapsed_ms={}; read_ms={read_ms}; materialization_ms={}; error={error}",
                            started.elapsed().as_millis(),
                            materialization_started.elapsed().as_millis(),
                        );
                        Err(InternalError::new_err(format!(
                            "Advanced result set but cursor description materialization failed: {error}"
                        )))
                    }
                }
            }
            Err(error) => {
                record_result_set_status("error");
                fetch_guard.fail(has_open_batch || connection_dead);
                future_fetch_state.set(FetchStatus::NoResultSet);
                Err(map_nextset_error(error, info_messages, read_ms))
            }
        }
    };
    let future = in_cursor_operation_span(future, cursor_id, operation_id, "nextset", "advancing");
    let future = async move {
        match dispatch {
            Some(dispatch) => future.with_subscriber(dispatch).await,
            None => future.await,
        }
    };
    let future = run_fetch_in_background(
        future,
        cancellation_state,
        operation_id,
        "nextset",
        cancellation_dispatch,
    );

    match pyo3_async_runtimes::tokio::future_into_py(py, future) {
        Ok(awaitable) => Ok(awaitable),
        Err(error) => {
            session_state.restore_fetch(operation_id);
            fetch_state.set(previous_fetch_status);
            description_state.replace(previous_description);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mssql_tds::error::Error;

    use super::{
        FetchGuard, MaterializationGuard, map_fetch_error, map_materialization_join_error,
        map_nextset_error,
    };
    use crate::async_session::{AsyncConnectionState, ClaimError, ConnectionLifecycle};

    fn claimed_fetch() -> (Arc<AsyncConnectionState>, u64) {
        let state = Arc::new(AsyncConnectionState::new());
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();
        (state, fetch.operation_id)
    }

    #[test]
    fn failed_fetch_releases_reusable_session_without_open_batch() {
        let (state, operation_id) = claimed_fetch();
        let mut guard = FetchGuard::new(Arc::clone(&state), operation_id, "fetchone", None);

        guard.fail(false);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Open);
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn failed_fetch_breaks_session_with_open_batch() {
        let (state, operation_id) = claimed_fetch();
        let mut guard = FetchGuard::new(Arc::clone(&state), operation_id, "fetchone", None);

        guard.fail(true);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Broken);
    }

    #[test]
    fn maps_fetch_protocol_error_to_python_internal_error() {
        let error = map_fetch_error(
            "fetchone",
            Error::ProtocolError("invalid row token".to_string()),
            Vec::new(),
            7,
        );

        pyo3::Python::attach(|py| {
            assert!(error.is_instance_of::<crate::async_errors::InternalError>(py));
        });
        assert!(error.to_string().contains(
            "PyAsyncCursor.fetchone failed while reading rows: Protocol Error: invalid row token"
        ));
    }

    #[test]
    fn maps_nextset_protocol_error_to_python_internal_error() {
        let error = map_nextset_error(
            Error::ProtocolError("invalid result token".to_string()),
            Vec::new(),
            7,
        );

        pyo3::Python::attach(|py| {
            assert!(error.is_instance_of::<crate::async_errors::InternalError>(py));
        });
        assert!(error.to_string().contains(
            "PyAsyncCursor.nextset failed while advancing results: Protocol Error: invalid result token"
        ));
    }

    #[test]
    fn dropping_interrupted_materialization_guard_is_safe() {
        drop(MaterializationGuard::new("fetchall", None));
    }

    #[tokio::test]
    async fn maps_materialization_task_panic_to_python_runtime_error() {
        let join_error = tokio::task::spawn_blocking(|| panic!("materialization failed"))
            .await
            .unwrap_err();

        let error = map_materialization_join_error(join_error);

        pyo3::Python::attach(|py| {
            assert!(error.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
        });
        assert!(
            error
                .to_string()
                .contains("Failed to materialize fetched rows: task")
        );
        assert!(error.to_string().contains("materialization failed"));
    }
}

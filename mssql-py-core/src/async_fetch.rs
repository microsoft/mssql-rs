// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous row fetching for [`crate::async_cursor::PyAsyncCursor`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::Instant;

use mssql_tds::connection::tds_client::StatementResult;
use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::error::{Error, SqlInfoMessage};
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

pub(crate) struct BufferedRowSet {
    pub(crate) metadata: Vec<mssql_tds::query::metadata::ColumnMetadata>,
    pub(crate) rows: VecDeque<PyRowWriter>,
}

#[derive(Default)]
pub(crate) struct BufferedResults(StdMutex<VecDeque<BufferedRowSet>>);

impl BufferedResults {
    pub(crate) fn replace(&self, results: VecDeque<BufferedRowSet>) -> VecDeque<BufferedRowSet> {
        std::mem::replace(&mut *self.lock(), results)
    }

    pub(crate) fn has_current(&self) -> bool {
        !self.lock().is_empty()
    }

    fn has_next(&self) -> bool {
        self.lock().len() > 1
    }

    fn take_rows(&self, limit: usize) -> (Vec<PyRowWriter>, bool, bool) {
        let mut results = self.lock();
        let Some(current) = results.front_mut() else {
            return (Vec::new(), true, false);
        };
        let count = limit.min(current.rows.len());
        let rows = current.rows.drain(..count).collect();
        let exhausted = current.rows.is_empty();
        let has_next = results.len() > 1;
        (rows, exhausted, has_next)
    }

    fn advance(&self) -> Option<Vec<mssql_tds::query::metadata::ColumnMetadata>> {
        let mut results = self.lock();
        results.pop_front();
        results.front().map(|result| result.metadata.clone())
    }

    fn lock(&self) -> StdMutexGuard<'_, VecDeque<BufferedRowSet>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
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
    buffered_results: Arc<BufferedResults>,
    rowcount: Arc<AtomicI64>,
}

impl FetchResources {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: Arc<Mutex<TdsClient>>,
        dispatch: Option<tracing::Dispatch>,
        session_state: Arc<AsyncConnectionState>,
        cursor_id: CursorId,
        fetch_state: Arc<FetchState>,
        description_state: Arc<DescriptionState>,
        buffered_results: Arc<BufferedResults>,
        rowcount: Arc<AtomicI64>,
    ) -> Self {
        Self {
            client,
            dispatch,
            session_state,
            cursor_id,
            fetch_state,
            description_state,
            buffered_results,
            rowcount,
        }
    }
}

/// Marks interrupted row reads as protocol-breaking operations.
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

    fn complete(&mut self, result_set_exhausted: bool, has_open_batch: bool) {
        self.session_state
            .finish_fetch(self.operation_id, result_set_exhausted, has_open_batch);
        self.completed = true;
    }

    fn fail(&mut self, has_open_batch: bool) {
        if has_open_batch {
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
    pyo3::exceptions::PyRuntimeError::new_err(format!(
        "Failed to materialize fetched rows: {error}"
    ))
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
        buffered_results,
        rowcount: _,
    } = resources;
    if buffered_results.has_current() {
        return fetch_buffered(
            cursor,
            py,
            dispatch,
            session_state,
            cursor_id,
            fetch_state,
            buffered_results,
            limit,
            output,
            operation,
        );
    }
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
    let guard_dispatch = dispatch.clone();
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

        let (result, info_messages, has_open_batch) = {
            let mut client = client.lock().await;
            let result = fetch_rows_on_client(&mut client, limit).await;
            let info_messages = if matches!(result, Err(Error::SqlServerError { .. })) {
                client.take_info_messages()
            } else {
                Vec::new()
            };
            let has_open_batch = client.has_open_batch();
            (result, info_messages, has_open_batch)
        };
        let read_ms = started.elapsed().as_millis();

        match result {
            Ok(batch) => {
                let returned = batch.rows.len();
                let exhausted = batch.exhausted;
                record_result_set_status(if exhausted { "exhausted" } else { "ready" });
                fetch_state.set(if batch.exhausted {
                    FetchStatus::Exhausted
                } else {
                    FetchStatus::Ready
                });
                fetch_guard.complete(batch.exhausted, has_open_batch);
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
                fetch_guard.fail(has_open_batch);
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

    match pyo3_async_runtimes::tokio::future_into_py(py, future) {
        Ok(awaitable) => Ok(awaitable),
        Err(error) => {
            session_state.restore_fetch(operation_id);
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fetch_buffered<'py>(
    cursor: Py<PyAsyncCursor>,
    py: Python<'py>,
    dispatch: Option<tracing::Dispatch>,
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    fetch_state: Arc<FetchState>,
    buffered_results: Arc<BufferedResults>,
    limit: usize,
    output: FetchOutput,
    operation: &'static str,
) -> PyResult<Bound<'py, PyAny>> {
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
    let guard_dispatch = dispatch.clone();

    let future = async move {
        let started = Instant::now();
        let _cursor = cursor;
        let mut fetch_guard =
            FetchGuard::new(future_state, operation_id, operation, guard_dispatch);
        let (rows, exhausted, has_next) = buffered_results.take_rows(limit);
        fetch_state.set(if exhausted {
            FetchStatus::Exhausted
        } else {
            FetchStatus::Ready
        });
        fetch_guard.complete(exhausted, !exhausted || has_next);
        let returned = rows.len();
        let rows = materialize_rows(output, rows).await?;
        let elapsed_ms = started.elapsed().as_millis();
        tracing::debug!(
            returned,
            exhausted,
            elapsed_ms,
            "PyAsyncCursor::{operation}: read buffered ExecuteMany rows"
        );
        Ok(rows)
    };
    let future = in_cursor_operation_span(future, cursor_id, operation_id, operation, "reading");
    let future = async move {
        match dispatch {
            Some(dispatch) => future.with_subscriber(dispatch).await,
            None => future.await,
        }
    };
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
        buffered_results,
        rowcount,
    } = resources;
    if buffered_results.has_current() {
        return next_buffered_set(
            py,
            dispatch,
            session_state,
            cursor_id,
            fetch_state,
            description_state,
            buffered_results,
            rowcount,
        );
    }
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
    let previous_fetch_status = fetch_state.replace(FetchStatus::NoResultSet);
    let previous_description = description_state.replace(None);
    let future_fetch_state = fetch_state.clone();
    let future_description_state = description_state.clone();
    let guard_dispatch = dispatch.clone();

    let future = async move {
        let started = Instant::now();
        tracing::debug!("PyAsyncCursor::nextset: started");
        let _cursor = cursor;
        let mut fetch_guard =
            FetchGuard::new(future_state, operation_id, "nextset", guard_dispatch);

        let (result, info_messages, has_open_batch) = {
            let mut client = client.lock().await;
            let result = client.advance().await.map(|result| {
                let metadata =
                    matches!(result, StatementResult::Rows).then(|| client.get_metadata().clone());
                (result, metadata)
            });
            let info_messages = if matches!(result, Err(Error::SqlServerError { .. })) {
                client.take_info_messages()
            } else {
                Vec::new()
            };
            (result, info_messages, client.has_open_batch())
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
                let next_rowcount = match result {
                    StatementResult::NoRows { rows_affected } => rows_affected
                        .and_then(|count| i64::try_from(count).ok())
                        .unwrap_or(-1),
                    StatementResult::Rows | StatementResult::End => -1,
                };

                let materialization_started = Instant::now();
                match materialize(metadata).await {
                    Ok(description) => {
                        future_description_state.replace(description);
                        future_fetch_state.set(next_fetch_status);
                        rowcount.store(next_rowcount, Ordering::Release);
                        fetch_guard.complete(!has_rows, has_open_batch);
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
                        fetch_guard.complete(true, has_open_batch);
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
                fetch_guard.fail(has_open_batch);
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

#[allow(clippy::too_many_arguments)]
fn next_buffered_set<'py>(
    py: Python<'py>,
    dispatch: Option<tracing::Dispatch>,
    session_state: Arc<AsyncConnectionState>,
    cursor_id: CursorId,
    fetch_state: Arc<FetchState>,
    description_state: Arc<DescriptionState>,
    buffered_results: Arc<BufferedResults>,
    rowcount: Arc<AtomicI64>,
) -> PyResult<Bound<'py, PyAny>> {
    if fetch_state.status() == FetchStatus::Exhausted && !buffered_results.has_next() {
        session_state.ensure_open().map_err(map_claim_error)?;
        buffered_results.advance();
        description_state.replace(None);
        rowcount.store(-1, Ordering::Release);
        return pyo3_async_runtimes::tokio::future_into_py(py, async { Ok(false) });
    }
    let claim = session_state
        .claim_fetch(cursor_id)
        .map_err(map_claim_error)?;
    let operation_id = claim.operation_id;
    let future_state = session_state.clone();
    let previous_fetch_status = fetch_state.replace(FetchStatus::NoResultSet);
    let previous_description = description_state.replace(None);
    let future_fetch_state = fetch_state.clone();
    let future_description_state = description_state.clone();
    let guard_dispatch = dispatch.clone();

    let future = async move {
        let mut fetch_guard =
            FetchGuard::new(future_state, operation_id, "nextset", guard_dispatch);
        let metadata = buffered_results.advance();
        let has_result = metadata.is_some();
        let description = match materialize(metadata).await {
            Ok(description) => description,
            Err(error) => {
                buffered_results.replace(VecDeque::new());
                future_fetch_state.set(FetchStatus::NoResultSet);
                fetch_guard.fail(false);
                return Err(InternalError::new_err(format!(
                    "Advanced buffered result set but cursor description materialization failed: {error}"
                )));
            }
        };
        future_description_state.replace(description);
        rowcount.store(-1, Ordering::Release);
        future_fetch_state.set(if has_result {
            FetchStatus::Ready
        } else {
            FetchStatus::Exhausted
        });
        fetch_guard.complete(true, has_result);
        Ok(has_result)
    };
    let future = in_cursor_operation_span(future, cursor_id, operation_id, "nextset", "advancing");
    let future = async move {
        match dispatch {
            Some(dispatch) => future.with_subscriber(dispatch).await,
            None => future.await,
        }
    };
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
    use crate::async_fetch::BufferedResults;
    use crate::async_session::{AsyncConnectionState, ClaimError, ConnectionLifecycle};

    fn claimed_fetch() -> (Arc<AsyncConnectionState>, u64) {
        let state = Arc::new(AsyncConnectionState::new());
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();
        (state, fetch.operation_id)
    }

    #[test]
    fn cleared_buffer_is_treated_as_exhausted() {
        let buffered_results = BufferedResults::default();

        let (rows, exhausted, has_next) = buffered_results.take_rows(usize::MAX);

        assert!(rows.is_empty());
        assert!(exhausted);
        assert!(!has_next);
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

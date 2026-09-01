// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous row fetching for [`crate::async_cursor::PyAsyncCursor`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

use mssql_tds::connection::tds_client::StatementResult;
use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::error::Error;
use pyo3::prelude::*;
use pyo3::types::PyList;
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use crate::async_cursor::{PyAsyncCursor, map_claim_error};
use crate::async_description::{DescriptionState, materialize};
use crate::async_session::{AsyncConnectionState, ClaimError, CursorId, OperationId};
use crate::row_writer::PyRowWriter;

const FETCH_YIELD_INTERVAL: usize = 256;
const FETCHALL_MATERIALIZE_CHUNK_SIZE: usize = 256;

#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum FetchStatus {
    NoResultSet,
    Ready,
    Exhausted,
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
            _ => FetchStatus::NoResultSet,
        }
    }

    pub(crate) fn replace(&self, status: FetchStatus) -> FetchStatus {
        let previous = self.0.swap(status as u8, Ordering::AcqRel);
        match previous {
            value if value == FetchStatus::Ready as u8 => FetchStatus::Ready,
            value if value == FetchStatus::Exhausted as u8 => FetchStatus::Exhausted,
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

    fn materialize(self, py: Python<'_>, rows: Vec<PyRowWriter>) -> PyResult<Py<PyAny>> {
        match self {
            Self::One => match rows.into_iter().next() {
                Some(writer) => Ok(writer.to_py_tuple(py)?.into_any().unbind()),
                None => Ok(py.None()),
            },
            Self::Many | Self::All => {
                let rows = rows
                    .into_iter()
                    .map(|writer| writer.to_py_tuple(py).map(Bound::unbind))
                    .collect::<PyResult<Vec<_>>>()?;
                Ok(PyList::new(py, rows)?.into_any().unbind())
            }
        }
    }
}

async fn materialize_rows(output: FetchOutput, rows: Vec<PyRowWriter>) -> PyResult<Py<PyAny>> {
    if matches!(output, FetchOutput::All) {
        return materialize_all_rows(rows).await;
    }
    tokio::task::spawn_blocking(move || Python::attach(|py| output.materialize(py, rows)))
        .await
        .map_err(map_materialization_join_error)?
}

async fn materialize_all_rows(rows: Vec<PyRowWriter>) -> PyResult<Py<PyAny>> {
    let mut rows = rows.into_iter();
    let mut list: Option<Py<PyList>> = None;

    loop {
        let chunk = rows
            .by_ref()
            .take(FETCHALL_MATERIALIZE_CHUNK_SIZE)
            .collect::<Vec<_>>();
        let finished = chunk.len() < FETCHALL_MATERIALIZE_CHUNK_SIZE;
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

fn map_fetch_error(operation: &str, error: Error, elapsed_ms: u128) -> PyErr {
    tracing::error!("PyAsyncCursor::{operation}: failed; elapsed_ms={elapsed_ms}; error={error}");
    pyo3::exceptions::PyRuntimeError::new_err(format!(
        "PyAsyncCursor.{operation} failed while reading rows: {error}"
    ))
}

fn map_nextset_error(error: Error, elapsed_ms: u128) -> PyErr {
    tracing::error!("PyAsyncCursor::nextset: failed; elapsed_ms={elapsed_ms}; error={error}");
    pyo3::exceptions::PyRuntimeError::new_err(format!(
        "PyAsyncCursor.nextset failed while advancing results: {error}"
    ))
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
    if fetch_state.status() == FetchStatus::NoResultSet {
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

        let (result, has_open_batch) = {
            let mut client = client.lock().await;
            let result = fetch_rows_on_client(&mut client, limit).await;
            let has_open_batch = client.has_open_batch();
            (result, has_open_batch)
        };
        let read_ms = started.elapsed().as_millis();

        match result {
            Ok(batch) => {
                let returned = batch.rows.len();
                let exhausted = batch.exhausted;
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
                fetch_guard.fail(has_open_batch);
                fetch_state.set(FetchStatus::NoResultSet);
                Err(map_fetch_error(
                    operation,
                    error,
                    started.elapsed().as_millis(),
                ))
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
        Err(ClaimError::NoResultSet) if fetch_state.status() == FetchStatus::Exhausted => {
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
    let materialization_dispatch = dispatch.clone();

    let future = async move {
        let started = Instant::now();
        tracing::debug!("PyAsyncCursor::nextset: started");
        let _cursor = cursor;
        let mut fetch_guard =
            FetchGuard::new(future_state, operation_id, "nextset", guard_dispatch);

        let (result, has_open_batch) = {
            let mut client = client.lock().await;
            let result = client.advance().await.map(|result| {
                let metadata =
                    matches!(result, StatementResult::Rows).then(|| client.get_metadata().clone());
                (result, metadata)
            });
            (result, client.has_open_batch())
        };
        let read_ms = started.elapsed().as_millis();

        match result {
            Ok((result, metadata)) => {
                let has_result = !matches!(result, StatementResult::End);
                let has_rows = matches!(result, StatementResult::Rows);
                let column_count = metadata.as_ref().map_or(0, Vec::len);
                future_fetch_state.set(match result {
                    StatementResult::Rows => FetchStatus::Ready,
                    StatementResult::NoRows { .. } => FetchStatus::NoResultSet,
                    StatementResult::End => FetchStatus::Exhausted,
                });
                fetch_guard.complete(!has_rows, has_open_batch);

                let materialization_started = Instant::now();
                let mut materialization_guard =
                    MaterializationGuard::new("nextset", materialization_dispatch);
                match materialize(metadata).await {
                    Ok(description) => {
                        materialization_guard.complete();
                        future_description_state.replace(description);
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
                        materialization_guard.complete();
                        tracing::error!(
                            "PyAsyncCursor::nextset: description materialization failed; column_count={column_count}; elapsed_ms={}; read_ms={read_ms}; materialization_ms={}; error={error}",
                            started.elapsed().as_millis(),
                            materialization_started.elapsed().as_millis(),
                        );
                        Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "Advanced result set but cursor description materialization failed: {error}"
                        )))
                    }
                }
            }
            Err(error) => {
                fetch_guard.fail(has_open_batch);
                future_fetch_state.set(FetchStatus::NoResultSet);
                Err(map_nextset_error(error, read_ms))
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

    use super::{FetchGuard, map_fetch_error, map_nextset_error};
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
    fn maps_fetch_error_to_python_runtime_error() {
        let error = map_fetch_error(
            "fetchone",
            Error::ProtocolError("invalid row token".to_string()),
            7,
        );

        assert!(error.to_string().contains(
            "PyAsyncCursor.fetchone failed while reading rows: Protocol Error: invalid row token"
        ));
    }

    #[test]
    fn maps_nextset_error_to_python_runtime_error() {
        let error = map_nextset_error(Error::ProtocolError("invalid result token".to_string()), 7);

        assert!(error.to_string().contains(
            "PyAsyncCursor.nextset failed while advancing results: Protocol Error: invalid result token"
        ));
    }
}

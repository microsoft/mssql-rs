// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous row fetching for [`crate::async_cursor::PyAsyncCursor`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::error::Error;
use pyo3::prelude::*;
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;

use crate::async_cursor::{PyAsyncCursor, map_claim_error};
use crate::async_session::{AsyncConnectionState, CursorId, OperationId};
use crate::row_writer::PyRowWriter;

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
}

impl FetchResources {
    pub(crate) fn new(
        client: Arc<Mutex<TdsClient>>,
        dispatch: Option<tracing::Dispatch>,
        session_state: Arc<AsyncConnectionState>,
        cursor_id: CursorId,
        fetch_state: Arc<FetchState>,
    ) -> Self {
        Self {
            client,
            dispatch,
            session_state,
            cursor_id,
            fetch_state,
        }
    }
}

/// Marks interrupted row reads as protocol-breaking operations.
struct FetchGuard {
    session_state: Arc<AsyncConnectionState>,
    operation_id: OperationId,
    completed: bool,
}

impl FetchGuard {
    fn new(session_state: Arc<AsyncConnectionState>, operation_id: OperationId) -> Self {
        Self {
            session_state,
            operation_id,
            completed: false,
        }
    }

    fn complete(&mut self, has_row: bool, has_open_batch: bool) {
        self.session_state
            .finish_fetch(self.operation_id, has_row, has_open_batch);
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
            self.session_state.mark_broken();
            self.session_state.release_operation(self.operation_id);
        }
    }
}

async fn fetch_one_on_client(client: &mut TdsClient) -> Result<Option<PyRowWriter>, Error> {
    if !client.on_rows() {
        return Ok(None);
    }

    let mut writer = PyRowWriter::new(client.get_metadata().len());
    if client.next_row_into(&mut writer).await? {
        Ok(Some(writer))
    } else {
        Ok(None)
    }
}

fn map_fetch_error(error: Error) -> PyErr {
    tracing::error!("PyAsyncCursor::fetchone: failed: {error}");
    pyo3::exceptions::PyRuntimeError::new_err(format!("Failed to fetch row: {error}"))
}

/// Return an awaitable resolving to the next row tuple or `None`.
pub(crate) fn fetchone<'py>(
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
    } = resources;
    if fetch_state.status() == FetchStatus::Exhausted {
        session_state.ensure_open().map_err(map_claim_error)?;
        return pyo3_async_runtimes::tokio::future_into_py(py, async move {
            Python::attach(|py| Ok(py.None()))
        });
    }
    let claim = session_state
        .claim_fetch(cursor_id)
        .map_err(map_claim_error)?;
    let operation_id = claim.operation_id;
    let future_state = session_state.clone();

    let future = async move {
        // Retain the Python cursor until the row operation settles so its finalizer
        // cannot race the in-flight TDS read.
        let _cursor = cursor;
        let mut fetch_guard = FetchGuard::new(future_state, operation_id);

        let (result, has_open_batch) = {
            let mut client = client.lock().await;
            let result = fetch_one_on_client(&mut client).await;
            let has_open_batch = client.has_open_batch();
            (result, has_open_batch)
        };

        match result {
            Ok(writer) => {
                fetch_guard.complete(writer.is_some(), has_open_batch);
                fetch_state.set(if writer.is_some() {
                    FetchStatus::Ready
                } else {
                    FetchStatus::Exhausted
                });
                if writer.is_none() {
                    tracing::info!("PyAsyncCursor::fetchone: result set exhausted");
                }
                Python::attach(|py| match writer {
                    Some(writer) => Ok(writer.to_py_tuple(py)?.into_any().unbind()),
                    None => Ok(py.None()),
                })
            }
            Err(error) => {
                fetch_guard.fail(has_open_batch);
                fetch_state.set(FetchStatus::NoResultSet);
                Err(map_fetch_error(error))
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mssql_tds::error::Error;

    use super::{FetchGuard, map_fetch_error};
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
        let mut guard = FetchGuard::new(Arc::clone(&state), operation_id);

        guard.fail(false);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Open);
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn failed_fetch_breaks_session_with_open_batch() {
        let (state, operation_id) = claimed_fetch();
        let mut guard = FetchGuard::new(Arc::clone(&state), operation_id);

        guard.fail(true);

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Broken);
    }

    #[test]
    fn maps_fetch_error_to_python_runtime_error() {
        let error = map_fetch_error(Error::ProtocolError("invalid row token".to_string()));

        assert!(
            error
                .to_string()
                .contains("Failed to fetch row: Protocol Error: invalid row token")
        );
    }
}

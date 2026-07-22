// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Async cursor — Phase 3: truly awaitable `execute_async` / `fetchone_async`.
//!
//! Futures returned to Python are driven by the shared Tokio runtime installed
//! in [`crate::async_runtime`]. The GIL is released for the duration of every
//! I/O `.await`; Python objects are only constructed inside `Python::attach`
//! at the tail of each future.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex as SyncMutex;
use pyo3::prelude::*;
use tokio::sync::Mutex;
use tracing::{error, info};

use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
use mssql_tds::core::CancelHandle;

use crate::row_writer::PyRowWriter;
use crate::utils::convert_tds_error;

/// Shared mutable cursor state, `Arc`-wrapped so `'static` futures produced by
/// `future_into_py` can own an inexpensive clone.
///
/// `active_cancel` uses a synchronous [`parking_lot::Mutex`] because it is
/// only ever touched off-await for a handful of ns; it is never held across
/// a `.await`. The `TdsClient` itself lives behind a [`tokio::sync::Mutex`]
/// that IS held across await points.
struct AsyncCursorState {
    has_resultset: AtomicBool,
    active_cancel: SyncMutex<Option<CancelHandle>>,
}

impl AsyncCursorState {
    fn new() -> Self {
        Self {
            has_resultset: AtomicBool::new(false),
            active_cancel: SyncMutex::new(None),
        }
    }
}

#[pyclass]
pub struct PyCoreAsyncCursor {
    tds_client: Arc<Mutex<TdsClient>>,
    state: Arc<AsyncCursorState>,
}

impl PyCoreAsyncCursor {
    pub(crate) fn new(tds_client: Arc<Mutex<TdsClient>>) -> Self {
        Self {
            tds_client,
            state: Arc::new(AsyncCursorState::new()),
        }
    }
}

#[pymethods]
impl PyCoreAsyncCursor {
    /// Cooperative cancel — sends a TDS attention for the currently in-flight
    /// operation, if any. Safe to call from another asyncio task while an
    /// `execute_async` or `fetchone_async` future is being awaited.
    ///
    /// After the future resumes and returns, the server-side cancellation
    /// surfaces as an `OperationCancelledError` mapped to a Python exception
    /// by [`convert_tds_error`].
    fn cancel(&self) {
        if let Some(handle) = self.state.active_cancel.lock().take() {
            info!("PyCoreAsyncCursor::cancel — dispatching TDS attention");
            handle.cancel();
        }
    }

    /// Execute a T-SQL batch. Returns a Python awaitable that resolves to
    /// `None` on success.
    #[pyo3(signature = (query, timeout_sec=30))]
    fn execute_async<'py>(
        &self,
        py: Python<'py>,
        query: String,
        timeout_sec: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let tds_client = self.tds_client.clone();
        let state = self.state.clone();

        // Install a fresh cancel handle BEFORE spawning the future so that a
        // concurrent `.cancel()` call can find it immediately.
        let parent = CancelHandle::new();
        let child = parent.child_handle();
        *state.active_cancel.lock() = Some(parent);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let _clear_on_return = ClearActiveCancelOnDrop(state.clone());
            let mut client = tds_client.lock().await;

            // Drain any lingering result set from a prior call before running
            // the next batch — matches the sync cursor's contract.
            if let Some(rs) = client.get_current_resultset() {
                info!("execute_async: closing stale result set");
                if let Err(e) = rs.close().await {
                    error!("execute_async: closing stale result set failed: {}", e);
                    return Err(convert_tds_error(e));
                }
            }

            info!("execute_async: executing query ({} chars)", query.len());
            client
                .execute(query, timeout_sec, Some(&child))
                .await
                .map_err(|e| {
                    error!("execute_async: execute failed: {}", e);
                    convert_tds_error(e)
                })?;

            state.has_resultset.store(true, Ordering::Release);
            Python::attach(|py| Ok(py.None()))
        })
    }

    /// Fetch the next row as a Python tuple, or `None` when the result set
    /// is exhausted. Idempotent once exhausted.
    fn fetchone_async<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let state = self.state.clone();

        // Fast path — no active result set, resolve to None without acquiring
        // the connection lock.
        if !state.has_resultset.load(Ordering::Acquire) {
            return pyo3_async_runtimes::tokio::future_into_py(py, async {
                Python::attach(|py| Ok(py.None()))
            });
        }

        let tds_client = self.tds_client.clone();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut client = tds_client.lock().await;

            // If the batch was closed between `has_resultset` check and lock
            // acquisition, resolve to None.
            let col_count = match client.get_current_resultset() {
                Some(rs) => rs.get_metadata().len(),
                None => {
                    state.has_resultset.store(false, Ordering::Release);
                    return Python::attach(|py| Ok(py.None()));
                }
            };

            // Rebuild the result-set borrow (dropped after `get_metadata` above).
            let rs = client
                .get_current_resultset()
                .expect("resultset present under held lock");

            let mut writer = PyRowWriter::new(col_count);
            let has_row = rs.next_row_into(&mut writer).await.map_err(|e| {
                error!("fetchone_async: next_row_into failed: {}", e);
                convert_tds_error(e)
            })?;

            if !has_row {
                if let Err(e) = rs.close().await {
                    error!("fetchone_async: close result set failed: {}", e);
                }
                state.has_resultset.store(false, Ordering::Release);
                return Python::attach(|py| Ok(py.None()));
            }

            Python::attach(|py| {
                let tup = writer.to_py_tuple(py)?;
                Ok::<Py<PyAny>, PyErr>(tup.into())
            })
        })
    }

    /// Best-effort close: clears the local result-set flag and dispatches a
    /// cancel if any operation is still in flight. Does not drop the
    /// connection.
    fn close(&self) -> PyResult<()> {
        self.state.has_resultset.store(false, Ordering::Release);
        if let Some(handle) = self.state.active_cancel.lock().take() {
            handle.cancel();
        }
        Ok(())
    }

    fn __repr__(&self) -> &'static str {
        "PyCoreAsyncCursor(ready)"
    }
}

/// Drop guard: on natural (non-panic) completion of an `execute_async` future,
/// clear the active cancel slot so a late `.cancel()` call becomes a no-op
/// instead of triggering a spurious attention against the next batch.
struct ClearActiveCancelOnDrop(Arc<AsyncCursorState>);
impl Drop for ClearActiveCancelOnDrop {
    fn drop(&mut self) {
        let _ = self.0.active_cancel.lock().take();
    }
}

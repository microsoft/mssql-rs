// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! First-class asynchronous (coroutine) cursor over the shared TDS core.
//!
//! [`PyCoreAsyncCursor`] shares the same [`SharedClient`] cell as the synchronous
//! [`PyCoreCursor`](crate::cursor::PyCoreCursor), but exposes genuine `asyncio`
//! coroutines: `execute`/`fetchone`/`fetchall`/`fetchmany`/`close` each return a
//! Python awaitable built by [`pyo3_async_runtimes::tokio::future_into_py`]. There
//! is no protocol-parsing duplication — both cursors drive the one shared token
//! and parse body; this cursor only ever uses the async [`TdsClient`] arm and
//! never flips to the reactor-free sync edge.
//!
//! Non-blocking discipline: the coroutine path does **no** `block_on`. The actual
//! TDS I/O (`next_row_into().await`, `execute().await`, …) is spawned onto the
//! owning connection's tokio runtime — the runtime its socket is registered with
//! — via [`Handle::spawn`], and the coroutine simply `.await`s the resulting
//! [`JoinHandle`](tokio::task::JoinHandle). While that awaits, the Python event
//! loop stays free to run other tasks, so a fetch never blocks the loop.
//!
//! Because the shared cell is backed by a [`std::sync::Mutex`] whose guard is
//! `!Send`, the spawned task uses [`pyclient::with_async_client`], which checks
//! the owned client out of the cell (dropping the guard before any `.await`) and
//! stores it back afterwards — even on error — so a mid-fetch failure leaves the
//! connection usable rather than poisoned.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, StatementResult, TdsClient};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyList;
use tokio::runtime::Handle;
use tokio::task::JoinError;

use crate::pyclient::{self, SharedClient};
use crate::row_writer::PyRowWriter;
use crate::utils::convert_tds_error;

/// Python asynchronous Cursor class driving the shared async TDS core.
#[pyclass]
pub struct PyCoreAsyncCursor {
    tds_client: SharedClient,
    runtime_handle: Handle,
    rowcount: Arc<AtomicI64>,
}

#[pymethods]
impl PyCoreAsyncCursor {
    #[pyo3(signature = (query, params=None))]
    #[allow(unused_variables)]
    fn execute<'py>(
        &self,
        py: Python<'py>,
        query: String,
        params: Option<Vec<Py<PyAny>>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cell = self.tds_client.clone();
        let handle = self.runtime_handle.clone();
        let rowcount = self.rowcount.clone();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            // Capture the rowcount oracle on the async client (rule C); the async
            // cursor never flips, so this is the same value the sync cursor sees.
            let count = handle
                .spawn(pyclient::with_async_client(cell, |mut client| async move {
                    let out = run_execute_on(&mut client, query).await;
                    (client, out)
                }))
                .await
                .map_err(join_err)??;
            rowcount.store(count, Ordering::SeqCst);
            Python::attach(|py| Ok(py.None()))
        })
    }

    fn fetchone<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cell = self.tds_client.clone();
        let handle = self.runtime_handle.clone();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let row = handle
                .spawn(pyclient::with_async_client(cell, |mut client| async move {
                    let out = fetch_one_on(&mut client).await;
                    (client, out)
                }))
                .await
                .map_err(join_err)??;
            Python::attach(|py| match row {
                Some(writer) => Ok(writer.to_py_tuple(py)?.into_any().unbind()),
                None => Ok(py.None()),
            })
        })
    }

    fn fetchall<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cell = self.tds_client.clone();
        let handle = self.runtime_handle.clone();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let rows = handle
                .spawn(pyclient::with_async_client(cell, |mut client| async move {
                    let out = fetch_all_on(&mut client).await;
                    (client, out)
                }))
                .await
                .map_err(join_err)??;
            Python::attach(|py| rows_to_py_list(py, &rows))
        })
    }

    #[pyo3(signature = (size=None))]
    fn fetchmany<'py>(&self, py: Python<'py>, size: Option<usize>) -> PyResult<Bound<'py, PyAny>> {
        let cell = self.tds_client.clone();
        let handle = self.runtime_handle.clone();
        let fetch_size = size.unwrap_or(1);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let rows = handle
                .spawn(pyclient::with_async_client(
                    cell,
                    move |mut client| async move {
                        let out = fetch_many_on(&mut client, fetch_size).await;
                        (client, out)
                    },
                ))
                .await
                .map_err(join_err)??;
            Python::attach(|py| rows_to_py_list(py, &rows))
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cell = self.tds_client.clone();
        let handle = self.runtime_handle.clone();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            handle
                .spawn(pyclient::with_async_client(cell, |mut client| async move {
                    let out = close_on(&mut client).await;
                    (client, out)
                }))
                .await
                .map_err(join_err)??;
            Python::attach(|py| Ok(py.None()))
        })
    }

    #[getter]
    fn rowcount(&self) -> i64 {
        self.rowcount.load(Ordering::SeqCst)
    }

    fn __repr__(&self) -> String {
        "PyCoreAsyncCursor()".to_string()
    }
}

impl PyCoreAsyncCursor {
    pub(crate) fn new(tds_client: SharedClient, runtime_handle: Handle) -> Self {
        Self {
            tds_client,
            runtime_handle,
            rowcount: Arc::new(AtomicI64::new(-1)),
        }
    }
}

/// Maps a spawned-task join failure (panic/cancellation) onto a Python error.
fn join_err(e: JoinError) -> PyErr {
    PyRuntimeError::new_err(format!("async cursor task failed: {e}"))
}

/// Builds a Python list of row tuples from decoded writers under the GIL.
fn rows_to_py_list<'py>(py: Python<'py>, rows: &[PyRowWriter]) -> PyResult<Py<PyAny>> {
    let list = PyList::empty(py);
    for writer in rows {
        list.append(writer.to_py_tuple(py)?)?;
    }
    Ok(list.into_any().unbind())
}

/// Runs a query on the async edge and collapses forward to the first
/// row-returning result set, returning the rowcount oracle captured on the async
/// client (identical to the sync cursor's value → parity by construction).
async fn run_execute_on(client: &mut TdsClient, query: String) -> Result<i64, PyErr> {
    if client.has_open_batch() {
        client.close_query().await.map_err(convert_tds_error)?;
    }
    let first = client
        .execute(
            query,
            ExecuteOptions {
                timeout: Some(30),
                ..Default::default()
            },
        )
        .await
        .map_err(convert_tds_error)?;
    if !matches!(first, StatementResult::Rows) {
        client.advance_to_rows().await.map_err(convert_tds_error)?;
    }
    Ok(client.last_rows_affected())
}

/// Pulls one row on the async edge, closing the result set at end-of-rows.
async fn fetch_one_on(client: &mut TdsClient) -> Result<Option<PyRowWriter>, PyErr> {
    if !client.on_rows() {
        return Ok(None);
    }
    let col_count = client.get_metadata().len();
    let mut writer = PyRowWriter::new(col_count);
    if client
        .next_row_into(&mut writer)
        .await
        .map_err(convert_tds_error)?
    {
        Ok(Some(writer))
    } else {
        client.close_query().await.map_err(convert_tds_error)?;
        Ok(None)
    }
}

/// Pulls up to `size` rows on the async edge, closing the result set if the row
/// stream is exhausted before `size` is reached.
async fn fetch_many_on(client: &mut TdsClient, size: usize) -> Result<Vec<PyRowWriter>, PyErr> {
    let mut rows = Vec::new();
    if !client.on_rows() {
        return Ok(rows);
    }
    let col_count = client.get_metadata().len();
    for _ in 0..size {
        let mut writer = PyRowWriter::new(col_count);
        if client
            .next_row_into(&mut writer)
            .await
            .map_err(convert_tds_error)?
        {
            rows.push(writer);
        } else {
            client.close_query().await.map_err(convert_tds_error)?;
            break;
        }
    }
    Ok(rows)
}

/// Drains the whole result set on the async edge into decoded row writers.
async fn fetch_all_on(client: &mut TdsClient) -> Result<Vec<PyRowWriter>, PyErr> {
    let mut rows = Vec::new();
    if !client.on_rows() {
        return Ok(rows);
    }
    let col_count = client.get_metadata().len();
    loop {
        let mut writer = PyRowWriter::new(col_count);
        if client
            .next_row_into(&mut writer)
            .await
            .map_err(convert_tds_error)?
        {
            rows.push(writer);
        } else {
            client.close_query().await.map_err(convert_tds_error)?;
            break;
        }
    }
    Ok(rows)
}

/// Closes the current result set on the async edge, if one is open.
async fn close_on(client: &mut TdsClient) -> Result<(), PyErr> {
    if client.has_open_batch() {
        client.close_query().await.map_err(convert_tds_error)?;
    }
    Ok(())
}

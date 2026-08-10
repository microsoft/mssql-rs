// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! First-class synchronous cursor over the reactor-free sync TDS core.
//!
//! [`PyCoreSyncCursor`] shares the same [`SharedClient`] cell as the default
//! block_on-backed [`PyCoreCursor`](crate::cursor::PyCoreCursor); it exposes an identical DB-API
//! surface but pulls rows through the reactor-free [`TdsSyncClient`] edge instead
//! of `block_on`-over-async. Control-plane work (execute/COLMETADATA/advance/
//! close) stays async on the shared core — only the SELECT row-pull hot loop
//! flips to the sync edge. There is no protocol-parsing duplication: both cursors
//! drive the one shared token/parse body.
//!
//! Flip discipline:
//! - `execute` runs fully async (rule C), captures the rowcount oracle, then
//!   flips to sync only when the statement is positioned on a row set.
//! - fetches use the sync edge (or, on a TLS/non-raw transport that reports
//!   `NotEligible`, transparently fall back to the async edge via `block_on`).
//! - any control-plane transition (next `execute`, end-of-rows `close`, cursor
//!   `close`, connection close) reverts to async first.

use pyo3::prelude::*;
use tokio::runtime::Handle;
use tracing::{error, info};

use crate::pyclient::{self, SharedClient};
use crate::row_writer::PyRowWriter;

/// Python synchronous Cursor class driving the reactor-free sync core.
#[pyclass]
pub struct PyCoreSyncCursor {
    tds_client: SharedClient,
    runtime_handle: Handle,
    has_resultset: bool,
    rowcount: i64,
}

#[pymethods]
impl PyCoreSyncCursor {
    #[pyo3(signature = (query, params=None))]
    #[allow(unused_variables)]
    fn execute(
        &mut self,
        py: Python,
        query: String,
        params: Option<Vec<Py<PyAny>>>,
    ) -> PyResult<()> {
        info!("sync execute: Executing query: {}", query);

        let cell = self.tds_client.clone();
        let handle = self.runtime_handle.clone();

        // Control-plane execute stays async so the rowcount oracle (DML count) is
        // captured on the async client before any flip to the sync edge.
        let (rowcount, on_rows) = py.detach(|| pyclient::run_execute(&cell, &handle, query, 30))?;

        if on_rows {
            // Flip to the reactor-free sync edge for the row-pull hot loop.
            // A TLS/non-raw transport reports NotEligible and stays async; the
            // fetch path then falls back to block_on transparently.
            py.detach(|| pyclient::flip_to_sync(&cell, &handle))?;
        }

        self.has_resultset = true;
        self.rowcount = rowcount;
        Ok(())
    }

    fn fetchone(&mut self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        if !self.has_resultset {
            return Ok(None);
        }

        let cell = self.tds_client.clone();
        let handle = self.runtime_handle.clone();

        let result: Option<PyRowWriter> = py.detach(|| {
            if !pyclient::is_on_rows(&cell)? {
                return Ok::<_, PyErr>(None);
            }

            let col_count = pyclient::metadata_col_count(&cell)?;
            let mut writer = PyRowWriter::new(col_count);

            match pyclient::fetch_row_into(&cell, &handle, &mut writer) {
                Ok(true) => Ok(Some(writer)),
                Ok(false) => {
                    // End of rows: revert to async and close the result set.
                    pyclient::close_resultset(&cell, &handle)?;
                    Ok(None)
                }
                Err(e) => {
                    error!("sync fetchone: fetch failed, reverting to async: {}", e);
                    // Recover the shared cell onto a live async edge; surface the
                    // original fetch error (do not poison silently, ruling 4).
                    let _ = pyclient::revert_to_async(&cell);
                    Err(e)
                }
            }
        })?;

        if let Some(writer) = result {
            Python::attach(|py| {
                let py_tuple = writer.to_py_tuple(py)?;
                Ok(Some(py_tuple.into()))
            })
        } else {
            self.has_resultset = false;
            Ok(None)
        }
    }

    fn fetchall(&mut self, py: Python) -> PyResult<Vec<Py<PyAny>>> {
        if !self.has_resultset {
            return Ok(vec![]);
        }

        let mut results = Vec::new();
        while let Some(row) = self.fetchone(py)? {
            results.push(row);
        }
        Ok(results)
    }

    fn fetchmany(&mut self, py: Python, size: Option<usize>) -> PyResult<Vec<Py<PyAny>>> {
        let fetch_size = size.unwrap_or(1);
        let mut results = Vec::new();

        for _ in 0..fetch_size {
            if let Some(row) = self.fetchone(py)? {
                results.push(row);
            } else {
                break;
            }
        }
        Ok(results)
    }

    fn close(&mut self, py: Python) -> PyResult<()> {
        self.has_resultset = false;
        let cell = self.tds_client.clone();
        // Revert any live sync edge so the connection can resume control-plane
        // work; best-effort, matching the default cursor's non-draining close.
        py.detach(|| {
            if let Err(e) = pyclient::revert_to_async(&cell) {
                error!("sync close: failed to revert to async edge: {}", e);
            }
        });
        Ok(())
    }

    #[getter]
    fn rowcount(&self) -> i64 {
        self.rowcount
    }

    fn __repr__(&self) -> String {
        "PyCoreSyncCursor()".to_string()
    }
}

impl PyCoreSyncCursor {
    pub(crate) fn new(tds_client: SharedClient, runtime_handle: Handle) -> Self {
        Self {
            tds_client,
            runtime_handle,
            has_resultset: false,
            rowcount: -1,
        }
    }
}

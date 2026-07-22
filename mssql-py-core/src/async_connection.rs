// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Async-first Connection class.
//!
//! Phase 2 POC: connection setup is performed synchronously on the shared
//! Tokio runtime (`block_on`), which keeps the API surface small and
//! matches the sync `PyCoreConnection` UX. Query-time work
//! (`execute_async`, `fetchone_async`) is truly async on the same shared
//! runtime — see [`crate::async_cursor::PyCoreAsyncCursor`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use tokio::sync::Mutex;

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;

use crate::async_cursor::PyCoreAsyncCursor;
use crate::async_runtime::shared_runtime;
use crate::connection::PyCoreConnection;

#[pyclass]
pub struct PyCoreAsyncConnection {
    tds_client: Arc<Mutex<TdsClient>>,
    is_closed: Arc<AtomicBool>,
}

#[pymethods]
impl PyCoreAsyncConnection {
    #[new]
    #[pyo3(signature = (client_context_dict, _python_logger=None))]
    fn new(
        py: Python<'_>,
        client_context_dict: &Bound<'_, PyDict>,
        _python_logger: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        // Reuse the existing sync connection's dict→ClientContext logic so
        // the async path stays byte-compatible with `bulkcopy`'s inputs.
        let ctx = PyCoreConnection::dict_to_client_context(client_context_dict)?;
        let datasource = ctx.data_source.clone();

        tracing::info!(
            "PyCoreAsyncConnection::new — connecting to datasource={}",
            datasource
        );

        // Drive the async connect on the shared runtime with the GIL released,
        // so other Python threads keep making progress during the handshake.
        let handle = shared_runtime().handle().clone();
        let client_result = py.detach(|| {
            handle.block_on(async move {
                let provider = TdsConnectionProvider {};
                provider.create_client(ctx, &datasource, None).await
            })
        });

        let client = client_result.map_err(|e| {
            tracing::error!("PyCoreAsyncConnection::new — connect failed: {}", e);
            PyRuntimeError::new_err(format!("Failed to connect to SQL Server: {e}"))
        })?;

        tracing::info!("PyCoreAsyncConnection::new — connected");
        Ok(Self {
            tds_client: Arc::new(Mutex::new(client)),
            is_closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Return a new async cursor sharing this connection's `TdsClient`.
    fn cursor(&self) -> PyResult<PyCoreAsyncCursor> {
        if self.is_closed.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Connection is closed"));
        }
        Ok(PyCoreAsyncCursor::new(self.tds_client.clone()))
    }

    /// Best-effort synchronous close.
    ///
    /// Sends the TDS close packet and tears down the TCP connection on the
    /// shared runtime. Idempotent: subsequent calls are no-ops.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        if self.is_closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let handle = shared_runtime().handle().clone();
        let client = self.tds_client.clone();
        py.detach(|| {
            handle.block_on(async move {
                let mut guard = client.lock().await;
                if let Err(e) = guard.close_connection().await {
                    tracing::warn!("PyCoreAsyncConnection::close — {}", e);
                }
            })
        });
        Ok(())
    }

    fn is_connected(&self) -> bool {
        !self.is_closed.load(Ordering::Acquire)
    }

    fn __repr__(&self) -> String {
        if self.is_closed.load(Ordering::Acquire) {
            "PyCoreAsyncConnection(closed)".to_string()
        } else {
            "PyCoreAsyncConnection(connected)".to_string()
        }
    }
}

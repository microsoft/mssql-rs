// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Async cursor — placeholder shell.
//!
//! Phase 2 adds a minimal `new(tds_client)` constructor so
//! `PyCoreAsyncConnection::cursor()` can hand out instances. The actual
//! `execute_async` / `fetchone_async` / `cancel` methods land in Phase 3.

use std::sync::Arc;

use pyo3::prelude::*;
use tokio::sync::Mutex;

use mssql_tds::connection::tds_client::TdsClient;

#[pyclass]
pub struct PyCoreAsyncCursor {
    #[allow(dead_code)] // wired up in Phase 3
    tds_client: Arc<Mutex<TdsClient>>,
}

impl PyCoreAsyncCursor {
    pub(crate) fn new(tds_client: Arc<Mutex<TdsClient>>) -> Self {
        Self { tds_client }
    }
}

#[pymethods]
impl PyCoreAsyncCursor {
    fn __repr__(&self) -> &'static str {
        "PyCoreAsyncCursor(pending Phase 3)"
    }
}

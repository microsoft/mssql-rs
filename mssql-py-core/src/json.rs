// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use pyo3::prelude::*;
use pyo3::types::PyString;

/// Internal bulk bridge for an already serialized, immutable JSON snapshot.
#[pyclass(frozen, module = "mssql_py_core")]
pub(crate) struct BulkCopyJson {
    pub(crate) text: Py<PyString>,
}

#[pymethods]
impl BulkCopyJson {
    #[new]
    fn new(text: Py<PyString>) -> Self {
        Self { text }
    }
}

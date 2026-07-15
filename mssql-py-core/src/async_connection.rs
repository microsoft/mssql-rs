// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Placeholder — the real implementation lands in Phase 2.
//!
//! Exists in Phase 1 only so `lib.rs` can register the class and the
//! extension exposes the expected symbol for the smoke-test.

use pyo3::prelude::*;

#[pyclass]
pub struct PyCoreAsyncConnection;

#[pymethods]
impl PyCoreAsyncConnection {
    fn __repr__(&self) -> &'static str {
        "PyCoreAsyncConnection(stub)"
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Placeholder — the real implementation lands in Phase 3.
//!
//! Exists in Phase 1 only so `lib.rs` can register the class and the
//! extension exposes the expected symbol for the smoke-test.

use pyo3::prelude::*;

#[pyclass]
pub struct PyCoreAsyncCursor;

#[pymethods]
impl PyCoreAsyncCursor {
    fn __repr__(&self) -> &'static str {
        "PyCoreAsyncCursor(stub)"
    }
}

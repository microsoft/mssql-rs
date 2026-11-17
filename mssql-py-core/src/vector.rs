// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::sql_vector::SqlVector;
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use mssql_tds::error::Error;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

/// Typed bridge for host drivers; unlike a list, retains the source SQL base type.
#[pyclass(frozen)]
pub(crate) struct BulkCopyVector {
    pub(crate) value: SqlVector,
}

#[pymethods]
impl BulkCopyVector {
    #[new]
    #[pyo3(signature = (values, base_type="float32"))]
    fn new(values: Vec<f32>, base_type: &str) -> PyResult<Self> {
        let base_type = match base_type {
            "float32" => VectorBaseType::Float32,
            "float16" => VectorBaseType::Float16,
            _ => return Err(PyValueError::new_err("Invalid vector base_type")),
        };
        Ok(Self {
            value: make_vector(values, base_type)
                .map_err(|e| PyValueError::new_err(e.to_string()))?,
        })
    }
}

pub(crate) fn make_vector(values: Vec<f32>, base_type: VectorBaseType) -> TdsResult<SqlVector> {
    for (index, value) in values.iter().enumerate() {
        if !value.is_finite() || (base_type == VectorBaseType::Float16 && value.abs() > 65504.0) {
            return Err(Error::UsageError(format!(
                "Vector element at index {index} is not finite or is outside the {base_type:?} range"
            )));
        }
    }
    match base_type {
        VectorBaseType::Float32 => SqlVector::try_from_f32(values),
        VectorBaseType::Float16 => SqlVector::try_from_f16(values),
    }
}

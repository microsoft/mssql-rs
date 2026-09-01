// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DB-API result-set metadata for the asynchronous cursor.

use std::sync::Mutex as StdMutex;

use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::query::metadata::ColumnMetadata;
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyBytes, PyDate, PyDateTime, PyFloat, PyInt, PyList, PyModule, PyString, PyTime,
    PyTuple, PyType,
};

/// Cursor-local Python snapshot of the current result-set metadata.
pub(crate) struct DescriptionState(StdMutex<Option<Py<PyList>>>);

impl DescriptionState {
    pub(crate) fn new() -> Self {
        Self(StdMutex::new(None))
    }

    pub(crate) fn replace(&self, description: Option<Py<PyList>>) -> Option<Py<PyList>> {
        std::mem::replace(
            &mut self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            description,
        )
    }

    pub(crate) fn get<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyList>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|description| description.clone_ref(py).into_bound(py))
    }
}

pub(crate) async fn materialize(
    metadata: Option<Vec<ColumnMetadata>>,
) -> PyResult<Option<Py<PyList>>> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || {
        Python::attach(|py| Ok(Some(description_to_python(py, &metadata)?.unbind())))
    })
    .await
    .map_err(|error| {
        pyo3::exceptions::PyRuntimeError::new_err(format!(
            "Failed to materialize cursor description: {error}"
        ))
    })?
}

fn python_type<'py>(py: Python<'py>, metadata: &ColumnMetadata) -> PyResult<Bound<'py, PyType>> {
    let python_type = match metadata.data_type {
        TdsDataType::Int1
        | TdsDataType::Int2
        | TdsDataType::Int4
        | TdsDataType::Int8
        | TdsDataType::IntN => py.get_type::<PyInt>(),
        TdsDataType::Bit | TdsDataType::BitN => py.get_type::<PyBool>(),
        TdsDataType::Flt4 | TdsDataType::Flt8 | TdsDataType::FltN => py.get_type::<PyFloat>(),
        TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN
        | TdsDataType::Money
        | TdsDataType::Money4
        | TdsDataType::MoneyN => imported_python_type(py, "decimal", "Decimal")?,
        TdsDataType::DateN => py.get_type::<PyDate>(),
        TdsDataType::TimeN => py.get_type::<PyTime>(),
        TdsDataType::DateTime
        | TdsDataType::DateTim4
        | TdsDataType::DateTimeN
        | TdsDataType::DateTime2N
        | TdsDataType::DateTimeOffsetN => py.get_type::<PyDateTime>(),
        TdsDataType::Binary
        | TdsDataType::VarBinary
        | TdsDataType::BigBinary
        | TdsDataType::BigVarBinary
        | TdsDataType::Image
        | TdsDataType::Udt => py.get_type::<PyBytes>(),
        TdsDataType::Guid => imported_python_type(py, "uuid", "UUID")?,
        _ => py.get_type::<PyString>(),
    };
    Ok(python_type)
}

fn imported_python_type<'py>(
    py: Python<'py>,
    module: &str,
    name: &str,
) -> PyResult<Bound<'py, PyType>> {
    Ok(PyModule::import(py, module)?
        .getattr(name)?
        .cast_into::<PyType>()?)
}

fn column_size(metadata: &ColumnMetadata) -> u64 {
    if metadata.is_plp() {
        return 0;
    }

    match metadata.data_type {
        TdsDataType::Int1 => 3,
        TdsDataType::Int2 => 5,
        TdsDataType::Int4 => 10,
        TdsDataType::Int8 => 19,
        TdsDataType::IntN => match metadata.type_info.length {
            1 => 3,
            2 => 5,
            4 => 10,
            8 => 19,
            _ => 0,
        },
        TdsDataType::Bit | TdsDataType::BitN => 1,
        TdsDataType::Flt4 => 7,
        TdsDataType::Flt8 => 15,
        TdsDataType::FltN => match metadata.type_info.length {
            4 => 7,
            8 => 15,
            _ => 0,
        },
        TdsDataType::DateN => 10,
        TdsDataType::TimeN => {
            let scale = u64::from(metadata.get_scale().unwrap_or(0));
            if scale == 0 { 8 } else { 9 + scale }
        }
        TdsDataType::DateTime => 23,
        TdsDataType::DateTim4 => 16,
        TdsDataType::DateTimeN => match metadata.type_info.length {
            8 => 23,
            4 => 16,
            _ => 0,
        },
        TdsDataType::DateTime2N => {
            let scale = u64::from(metadata.get_scale().unwrap_or(0));
            if scale == 0 { 19 } else { 20 + scale }
        }
        TdsDataType::DateTimeOffsetN => {
            let scale = u64::from(metadata.get_scale().unwrap_or(0));
            if scale == 0 { 26 } else { 27 + scale }
        }
        TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN
        | TdsDataType::Money
        | TdsDataType::Money4
        | TdsDataType::MoneyN => u64::from(metadata.get_precision().unwrap_or(0)),
        TdsDataType::NChar | TdsDataType::NVarChar | TdsDataType::NText => {
            (metadata.type_info.length / 2) as u64
        }
        _ => metadata.type_info.length as u64,
    }
}

fn decimal_digits(metadata: &ColumnMetadata) -> u8 {
    match metadata.data_type {
        TdsDataType::Money | TdsDataType::Money4 | TdsDataType::MoneyN => 4,
        TdsDataType::DateTime => 3,
        TdsDataType::DateTim4 => 0,
        TdsDataType::DateTimeN => match metadata.type_info.length {
            8 => 3,
            _ => 0,
        },
        _ => metadata.get_scale().unwrap_or(0),
    }
}

fn description_to_python<'py>(
    py: Python<'py>,
    metadata: &[ColumnMetadata],
) -> PyResult<Bound<'py, PyList>> {
    let mut description = Vec::with_capacity(metadata.len());
    for column in metadata {
        let size = column_size(column);
        description.push(PyTuple::new(
            py,
            [
                column.column_name.clone().into_pyobject(py)?.into_any(),
                python_type(py, column)?.into_any(),
                py.None().into_bound(py),
                size.into_pyobject(py)?.into_any(),
                size.into_pyobject(py)?.into_any(),
                decimal_digits(column).into_pyobject(py)?.into_any(),
                column
                    .is_nullable()
                    .into_pyobject(py)?
                    .to_owned()
                    .into_any(),
            ],
        )?);
    }
    PyList::new(py, description)
}

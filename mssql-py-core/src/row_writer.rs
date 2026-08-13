// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::row_writer::{LenHint, RowWriter, ValueKind};
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sql_vector::SqlVector;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use uuid::Uuid;

use crate::cursor::PyCoreCursor;

/// Accumulates decoded TDS values for one row and materializes them as
/// a Python tuple when the GIL is available.
///
/// During the async TDS decode (GIL released), values are stored as
/// `ColumnValues`. After re-acquiring the GIL, `to_py_tuple()` converts
/// everything to Python objects in a single pass.
pub(crate) struct PyRowWriter {
    row: Vec<ColumnValues>,
    scratch: Vec<u8>,
    pending: Option<ValueKind>,
}

impl PyRowWriter {
    pub fn new(col_count: usize) -> Self {
        Self {
            row: Vec::with_capacity(col_count),
            scratch: Vec::new(),
            pending: None,
        }
    }

    pub fn to_py_tuple<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let py_values: Vec<Bound<'py, PyAny>> = self
            .row
            .iter()
            .map(|col_val| PyCoreCursor::column_value_to_python(py, col_val))
            .collect();
        PyTuple::new(py, py_values.iter())
    }
}

impl RowWriter for PyRowWriter {
    fn write_null(&mut self, _col: usize) {
        self.row.push(ColumnValues::Null);
    }

    fn write_bool(&mut self, _col: usize, val: bool) {
        self.row.push(ColumnValues::Bit(val));
    }

    fn write_u8(&mut self, _col: usize, val: u8) {
        self.row.push(ColumnValues::TinyInt(val));
    }

    fn write_i16(&mut self, _col: usize, val: i16) {
        self.row.push(ColumnValues::SmallInt(val));
    }

    fn write_i32(&mut self, _col: usize, val: i32) {
        self.row.push(ColumnValues::Int(val));
    }

    fn write_i64(&mut self, _col: usize, val: i64) {
        self.row.push(ColumnValues::BigInt(val));
    }

    fn write_f32(&mut self, _col: usize, val: f32) {
        self.row.push(ColumnValues::Real(val));
    }

    fn write_f64(&mut self, _col: usize, val: f64) {
        self.row.push(ColumnValues::Float(val));
    }

    fn write_str(&mut self, _col: usize, bytes: &[u8], encoding: EncodingType) {
        self.row
            .push(ColumnValues::String(SqlString::new(bytes.to_vec(), encoding)));
    }

    fn write_blob(&mut self, _col: usize, bytes: &[u8]) {
        self.row.push(ColumnValues::Bytes(bytes.to_vec()));
    }

    fn write_decimal(&mut self, _col: usize, val: DecimalParts) {
        self.row.push(ColumnValues::Decimal(val));
    }

    fn write_numeric(&mut self, _col: usize, val: DecimalParts) {
        self.row.push(ColumnValues::Numeric(val));
    }

    fn write_date(&mut self, _col: usize, val: SqlDate) {
        self.row.push(ColumnValues::Date(val));
    }

    fn write_time(&mut self, _col: usize, val: SqlTime) {
        self.row.push(ColumnValues::Time(val));
    }

    fn write_datetime(&mut self, _col: usize, val: SqlDateTime) {
        self.row.push(ColumnValues::DateTime(val));
    }

    fn write_smalldatetime(&mut self, _col: usize, val: SqlSmallDateTime) {
        self.row.push(ColumnValues::SmallDateTime(val));
    }

    fn write_datetime2(&mut self, _col: usize, val: SqlDateTime2) {
        self.row.push(ColumnValues::DateTime2(val));
    }

    fn write_datetimeoffset(&mut self, _col: usize, val: SqlDateTimeOffset) {
        self.row.push(ColumnValues::DateTimeOffset(val));
    }

    fn write_money(&mut self, _col: usize, val: SqlMoney) {
        self.row.push(ColumnValues::Money(val));
    }

    fn write_smallmoney(&mut self, _col: usize, val: SqlSmallMoney) {
        self.row.push(ColumnValues::SmallMoney(val));
    }

    fn write_uuid(&mut self, _col: usize, val: Uuid) {
        self.row.push(ColumnValues::Uuid(val));
    }

    fn write_xml(&mut self, _col: usize, val: SqlXml) {
        self.row.push(ColumnValues::Xml(val));
    }

    fn write_json(&mut self, _col: usize, val: SqlJson) {
        self.row.push(ColumnValues::Json(val));
    }

    fn write_vector(&mut self, _col: usize, val: SqlVector) {
        self.row.push(ColumnValues::Vector(val));
    }

    fn begin_value(&mut self, _col: usize, kind: ValueKind, hint: LenHint) {
        /// Declared PLP lengths reach ~2 GiB; treat them as a sizing hint, not an order.
        const PREALLOC_CAP: usize = 1 << 20;
        if let LenHint::Exact(n) = hint {
            self.scratch.reserve(n.min(PREALLOC_CAP));
        }
        self.pending = Some(kind);
    }

    fn reserve(&mut self, _col: usize, n: usize) -> &mut [u8] {
        let at = self.scratch.len();
        self.scratch.resize(at + n, 0);
        &mut self.scratch[at..]
    }

    fn commit_value(&mut self, _col: usize) {
        let kind = self
            .pending
            .take()
            .expect("commit_value without begin_value");
        let bytes = std::mem::take(&mut self.scratch);
        self.row.push(match kind {
            ValueKind::Str(enc) => ColumnValues::String(SqlString::new(bytes, enc)),
            ValueKind::Blob => ColumnValues::Bytes(bytes),
        });
    }

    fn end_row(&mut self) {
        // No-op — caller takes the row after each decode cycle.
    }

    fn abandon_row(&mut self) {
        self.pending = None;
        self.scratch.clear();
        self.row.clear();
    }
}

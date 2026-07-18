// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::row_writer::RowWriter;
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_string::SqlString;
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
}

impl PyRowWriter {
    pub fn new(col_count: usize) -> Self {
        Self {
            row: Vec::with_capacity(col_count),
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

    fn write_string(&mut self, _col: usize, val: SqlString) {
        self.row.push(ColumnValues::String(val));
    }

    fn write_bytes(&mut self, _col: usize, val: Vec<u8>) {
        self.row.push(ColumnValues::Bytes(val));
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

    fn end_row(&mut self) {
        // No-op — caller takes the row after each decode cycle.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_methods_accumulate_column_values() {
        let mut w = PyRowWriter::new(0);
        w.write_null(0);
        w.write_bool(0, true);
        w.write_u8(0, 7);
        w.write_i16(0, -3);
        w.write_i32(0, 42);
        w.write_i64(0, 1 << 40);
        w.write_f32(0, 1.5);
        w.write_f64(0, 2.5);
        w.write_string(0, SqlString::from_utf8_string("hi".into()));
        w.write_bytes(0, vec![1, 2, 3]);
        w.write_decimal(0, DecimalParts::from_string("1.5", 2, 1).unwrap());
        w.write_numeric(0, DecimalParts::from_string("2.5", 2, 1).unwrap());
        w.write_date(0, SqlDate::create(1).unwrap());
        let time = SqlTime {
            time_nanoseconds: 1,
            scale: 7,
        };
        w.write_time(0, time.clone());
        w.write_datetime(0, SqlDateTime { days: 1, time: 2 });
        w.write_smalldatetime(0, SqlSmallDateTime { days: 1, time: 2 });
        let dt2 = SqlDateTime2 {
            days: 1,
            time: time.clone(),
        };
        w.write_datetime2(0, dt2.clone());
        w.write_datetimeoffset(
            0,
            SqlDateTimeOffset {
                datetime2: dt2,
                offset: 60,
            },
        );
        w.write_money(0, SqlMoney::from(10_000));
        w.write_smallmoney(0, SqlSmallMoney::from(5_000));
        w.write_uuid(0, Uuid::from_u128(0));
        w.write_xml(0, SqlXml::from("<x/>".to_string()));
        w.write_json(0, SqlJson::new(b"{}".to_vec()));
        w.write_vector(0, SqlVector::try_from_f32(vec![1.0, 2.0]).unwrap());
        w.end_row();

        assert!(matches!(w.row[0], ColumnValues::Null));
        assert!(matches!(w.row[1], ColumnValues::Bit(true)));
        assert!(matches!(w.row[2], ColumnValues::TinyInt(7)));
        assert!(matches!(w.row[3], ColumnValues::SmallInt(-3)));
        assert!(matches!(w.row[4], ColumnValues::Int(42)));
        assert!(matches!(w.row[5], ColumnValues::BigInt(_)));
        assert!(matches!(w.row[6], ColumnValues::Real(_)));
        assert!(matches!(w.row[7], ColumnValues::Float(_)));
        assert!(matches!(w.row[8], ColumnValues::String(_)));
        assert!(matches!(w.row[9], ColumnValues::Bytes(_)));
        assert!(matches!(w.row[10], ColumnValues::Decimal(_)));
        assert!(matches!(w.row[11], ColumnValues::Numeric(_)));
        assert!(matches!(w.row[12], ColumnValues::Date(_)));
        assert!(matches!(w.row[13], ColumnValues::Time(_)));
        assert!(matches!(w.row[14], ColumnValues::DateTime(_)));
        assert!(matches!(w.row[15], ColumnValues::SmallDateTime(_)));
        assert!(matches!(w.row[16], ColumnValues::DateTime2(_)));
        assert!(matches!(w.row[17], ColumnValues::DateTimeOffset(_)));
        assert!(matches!(w.row[18], ColumnValues::Money(_)));
        assert!(matches!(w.row[19], ColumnValues::SmallMoney(_)));
        assert!(matches!(w.row[20], ColumnValues::Uuid(_)));
        assert!(matches!(w.row[21], ColumnValues::Xml(_)));
        assert!(matches!(w.row[22], ColumnValues::Json(_)));
        assert!(matches!(w.row[23], ColumnValues::Vector(_)));
        assert_eq!(w.row.len(), 24);
    }
}

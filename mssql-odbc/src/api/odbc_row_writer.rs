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

/// ODBC-oriented row writer that supports pausing row decode after a requested
/// column while preserving already materialized columns.
#[derive(Debug)]
pub(crate) struct OdbcRowWriter {
    row: Vec<ColumnValues>,
    pause_before_first_column: bool,
    pause_after_column: Option<usize>,
    row_complete: bool,
}

impl OdbcRowWriter {
    pub(crate) fn new(col_count: usize) -> Self {
        Self {
            row: Vec::with_capacity(col_count),
            pause_before_first_column: false,
            pause_after_column: None,
            row_complete: false,
        }
    }

    pub(crate) fn from_row(row: Vec<ColumnValues>, col_count: usize) -> Self {
        let mut writer = Self::new(col_count);
        writer.row = row;
        writer
    }

    pub(crate) fn request_pause_after_column(&mut self, column_number: usize) {
        self.pause_after_column = Some(column_number);
    }

    pub(crate) fn request_pause_before_first_column(&mut self) {
        self.pause_before_first_column = true;
    }

    pub(crate) fn into_row(self) -> Vec<ColumnValues> {
        self.row
    }

    pub(crate) fn row_complete(&self) -> bool {
        self.row_complete
    }

    fn set_column(&mut self, col: usize, value: ColumnValues) {
        if col < self.row.len() {
            self.row[col] = value;
            return;
        }

        debug_assert_eq!(
            col,
            self.row.len(),
            "RowWriter emitted non-sequential column index"
        );
        self.row.push(value);
    }
}

impl RowWriter for OdbcRowWriter {
    fn pause_before_first_column(&self) -> bool {
        self.pause_before_first_column
    }

    fn pause_after_column(&self, col: usize) -> bool {
        self.pause_after_column == Some(col + 1)
    }

    fn write_null(&mut self, col: usize) {
        self.set_column(col, ColumnValues::Null);
    }

    fn write_bool(&mut self, col: usize, val: bool) {
        self.set_column(col, ColumnValues::Bit(val));
    }

    fn write_u8(&mut self, col: usize, val: u8) {
        self.set_column(col, ColumnValues::TinyInt(val));
    }

    fn write_i16(&mut self, col: usize, val: i16) {
        self.set_column(col, ColumnValues::SmallInt(val));
    }

    fn write_i32(&mut self, col: usize, val: i32) {
        self.set_column(col, ColumnValues::Int(val));
    }

    fn write_i64(&mut self, col: usize, val: i64) {
        self.set_column(col, ColumnValues::BigInt(val));
    }

    fn write_f32(&mut self, col: usize, val: f32) {
        self.set_column(col, ColumnValues::Real(val));
    }

    fn write_f64(&mut self, col: usize, val: f64) {
        self.set_column(col, ColumnValues::Float(val));
    }

    fn write_string(&mut self, col: usize, val: SqlString) {
        self.set_column(col, ColumnValues::String(val));
    }

    fn write_bytes(&mut self, col: usize, val: Vec<u8>) {
        self.set_column(col, ColumnValues::Bytes(val));
    }

    fn write_decimal(&mut self, col: usize, val: DecimalParts) {
        self.set_column(col, ColumnValues::Decimal(val));
    }

    fn write_numeric(&mut self, col: usize, val: DecimalParts) {
        self.set_column(col, ColumnValues::Numeric(val));
    }

    fn write_date(&mut self, col: usize, val: SqlDate) {
        self.set_column(col, ColumnValues::Date(val));
    }

    fn write_time(&mut self, col: usize, val: SqlTime) {
        self.set_column(col, ColumnValues::Time(val));
    }

    fn write_datetime(&mut self, col: usize, val: SqlDateTime) {
        self.set_column(col, ColumnValues::DateTime(val));
    }

    fn write_smalldatetime(&mut self, col: usize, val: SqlSmallDateTime) {
        self.set_column(col, ColumnValues::SmallDateTime(val));
    }

    fn write_datetime2(&mut self, col: usize, val: SqlDateTime2) {
        self.set_column(col, ColumnValues::DateTime2(val));
    }

    fn write_datetimeoffset(&mut self, col: usize, val: SqlDateTimeOffset) {
        self.set_column(col, ColumnValues::DateTimeOffset(val));
    }

    fn write_money(&mut self, col: usize, val: SqlMoney) {
        self.set_column(col, ColumnValues::Money(val));
    }

    fn write_smallmoney(&mut self, col: usize, val: SqlSmallMoney) {
        self.set_column(col, ColumnValues::SmallMoney(val));
    }

    fn write_uuid(&mut self, col: usize, val: uuid::Uuid) {
        self.set_column(col, ColumnValues::Uuid(val));
    }

    fn write_xml(&mut self, col: usize, val: SqlXml) {
        self.set_column(col, ColumnValues::Xml(val));
    }

    fn write_json(&mut self, col: usize, val: SqlJson) {
        self.set_column(col, ColumnValues::Json(val));
    }

    fn write_vector(&mut self, col: usize, val: SqlVector) {
        self.set_column(col, ColumnValues::Vector(val));
    }

    fn end_row(&mut self) {
        self.row_complete = true;
        self.pause_before_first_column = false;
        self.pause_after_column = None;
    }
}

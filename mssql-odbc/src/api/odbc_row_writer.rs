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
use mssql_tds::core::TdsResult;
use mssql_tds::token::tokens::SqlCollation;

use super::odbc_types::SQL_C_WCHAR;

/// ODBC-oriented row writer that supports pausing row decode after a requested
/// column while preserving already materialized columns.
#[derive(Debug)]
pub(crate) struct OdbcRowWriter {
    row: Vec<ColumnValues>,
    pause_after_column: Option<usize>,
    row_complete: bool,
    active_plp_text: Option<String>,
    active_plp_collation: Option<SqlCollation>,
    active_plp_target_type: Option<i16>,
    active_plp_offset: usize,
}

impl OdbcRowWriter {
    pub(crate) fn new(col_count: usize) -> Self {
        Self {
            row: Vec::with_capacity(col_count),
            pause_after_column: None,
            row_complete: false,
            active_plp_text: None,
            active_plp_collation: None,
            active_plp_target_type: None,
            active_plp_offset: 0,
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

    pub(crate) fn into_row(self) -> Vec<ColumnValues> {
        self.row
    }

    pub(crate) fn row_complete(&self) -> bool {
        self.row_complete
    }

    pub(crate) fn set_active_plp_text(&mut self, decoded: String, collation: Option<SqlCollation>) {
        self.active_plp_text = Some(decoded);
        self.active_plp_collation = collation;
        self.active_plp_target_type = None;
        self.active_plp_offset = 0;
    }

    pub(crate) fn set_active_plp_target_type(&mut self, target_type: i16) {
        self.active_plp_target_type = Some(target_type);
    }

    pub(crate) fn set_active_plp_offset(&mut self, offset: usize) {
        self.active_plp_offset = offset;
    }

    pub(crate) fn active_plp_offset(&self) -> usize {
        self.active_plp_offset
    }

    pub(crate) fn active_plp_remaining_len(&self) -> usize {
        let bytes = self.active_plp_encoded_bytes();
        let start = self.active_plp_offset.min(bytes.len());
        bytes.len().saturating_sub(start)
    }

    fn active_plp_encoded_bytes(&self) -> Vec<u8> {
        let Some(text) = self.active_plp_text.as_ref() else {
            return Vec::new();
        };

        match self.active_plp_target_type {
            Some(SQL_C_WCHAR) => text
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect(),
            _ => text.as_bytes().to_vec(),
        }
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
    fn pause_after_column(&self, col: usize) -> bool {
        self.pause_after_column == Some(col + 1)
    }

    fn read_active_plp_bytes(&mut self, out: &mut [u8]) -> TdsResult<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        let bytes = self.active_plp_encoded_bytes();
        let start = self.active_plp_offset.min(bytes.len());
        let mut copy_len = out.len().min(bytes.len().saturating_sub(start));

        if self.active_plp_target_type == Some(SQL_C_WCHAR) && (copy_len % 2 != 0) {
            copy_len -= 1;
        }

        if copy_len == 0 {
            return Ok(0);
        }

        out[..copy_len].copy_from_slice(&bytes[start..start + copy_len]);
        self.active_plp_offset = start + copy_len;
        Ok(copy_len)
    }

    fn active_plp_reached_end(&self) -> bool {
        let bytes = self.active_plp_encoded_bytes();
        self.active_plp_offset >= bytes.len()
    }

    fn active_plp_collation(&self) -> Option<SqlCollation> {
        self.active_plp_collation
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
        self.pause_after_column = None;
    }
}

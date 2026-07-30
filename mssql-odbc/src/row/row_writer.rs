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
use uuid::Uuid;

/// Wire encoding of a PLP column; used to select and transcode the delivered
/// SQL C type. UTF-16 text can be delivered as SQL_C_WCHAR or transcoded to
/// SQL_C_CHAR; single-byte text as SQL_C_CHAR. Binary delivery and
/// varchar->SQL_C_WCHAR widening are not yet supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlpEncoding {
    /// nvarchar(max), nchar(max), xml — UTF-16LE on the wire.
    Utf16Text,
    /// varchar(max), text, json — single-byte / UTF-8 on the wire.
    SingleByteText,
    /// varbinary(max), image, UDT — opaque bytes.
    Binary,
}

/// Column-wise row sink for the ODBC fetch/get-data path.
///
/// pause_before_first_column is always true: SQLFetch positions on a row
/// without materializing any column. pause_after_column fires on the single
/// column requested via request(), capturing its value; all other columns are
/// decoded by the TDS layer and discarded.
#[derive(Default)]
pub(crate) struct OdbcRowWriter {
    /// 0-based index of the column to capture. None = position/drain mode.
    requested_col: Option<usize>,
    /// Captured value for requested_col (non-PLP columns only).
    captured: Option<ColumnValues>,
    /// Set once the decoder completes the row via end_row.
    end_row_fired: bool,
}

impl OdbcRowWriter {
    /// Creates a writer in position/drain mode (no column requested).
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Requests capture of 0-based col, resetting per-call state.
    pub(crate) fn request(&mut self, col: usize) {
        self.requested_col = Some(col);
        self.captured = None;
        self.end_row_fired = false;
    }

    /// Takes the captured value for the requested column, if one was decoded.
    pub(crate) fn take_captured(&mut self) -> Option<ColumnValues> {
        self.captured.take()
    }

    /// Returns true if end_row fired during the last next_row_into call.
    pub(crate) fn end_row_fired(&self) -> bool {
        self.end_row_fired
    }

    #[inline]
    fn capture(&mut self, col: usize, value: ColumnValues) {
        if self.requested_col == Some(col) {
            self.captured = Some(value);
        }
    }
}

impl RowWriter for OdbcRowWriter {
    fn pause_before_first_column(&self) -> bool {
        // True when in position/drain mode (SQLFetch, no column requested).
        // False when resuming to a specific column (SQLGetData).
        self.requested_col.is_none()
    }

    fn pause_after_column(&self, col: usize) -> bool {
        self.requested_col == Some(col)
    }

    fn write_null(&mut self, col: usize) {
        self.capture(col, ColumnValues::Null);
    }
    fn write_bool(&mut self, col: usize, val: bool) {
        self.capture(col, ColumnValues::Bit(val));
    }
    fn write_u8(&mut self, col: usize, val: u8) {
        self.capture(col, ColumnValues::TinyInt(val));
    }
    fn write_i16(&mut self, col: usize, val: i16) {
        self.capture(col, ColumnValues::SmallInt(val));
    }
    fn write_i32(&mut self, col: usize, val: i32) {
        self.capture(col, ColumnValues::Int(val));
    }
    fn write_i64(&mut self, col: usize, val: i64) {
        self.capture(col, ColumnValues::BigInt(val));
    }
    fn write_f32(&mut self, col: usize, val: f32) {
        self.capture(col, ColumnValues::Real(val));
    }
    fn write_f64(&mut self, col: usize, val: f64) {
        self.capture(col, ColumnValues::Float(val));
    }
    fn write_string(&mut self, col: usize, val: SqlString) {
        self.capture(col, ColumnValues::String(val));
    }
    fn write_bytes(&mut self, col: usize, val: Vec<u8>) {
        self.capture(col, ColumnValues::Bytes(val));
    }
    fn write_decimal(&mut self, col: usize, val: DecimalParts) {
        self.capture(col, ColumnValues::Decimal(val));
    }
    fn write_numeric(&mut self, col: usize, val: DecimalParts) {
        self.capture(col, ColumnValues::Numeric(val));
    }
    fn write_date(&mut self, col: usize, val: SqlDate) {
        self.capture(col, ColumnValues::Date(val));
    }
    fn write_time(&mut self, col: usize, val: SqlTime) {
        self.capture(col, ColumnValues::Time(val));
    }
    fn write_datetime(&mut self, col: usize, val: SqlDateTime) {
        self.capture(col, ColumnValues::DateTime(val));
    }
    fn write_smalldatetime(&mut self, col: usize, val: SqlSmallDateTime) {
        self.capture(col, ColumnValues::SmallDateTime(val));
    }
    fn write_datetime2(&mut self, col: usize, val: SqlDateTime2) {
        self.capture(col, ColumnValues::DateTime2(val));
    }
    fn write_datetimeoffset(&mut self, col: usize, val: SqlDateTimeOffset) {
        self.capture(col, ColumnValues::DateTimeOffset(val));
    }
    fn write_money(&mut self, col: usize, val: SqlMoney) {
        self.capture(col, ColumnValues::Money(val));
    }
    fn write_smallmoney(&mut self, col: usize, val: SqlSmallMoney) {
        self.capture(col, ColumnValues::SmallMoney(val));
    }
    fn write_uuid(&mut self, col: usize, val: Uuid) {
        self.capture(col, ColumnValues::Uuid(val));
    }
    fn write_xml(&mut self, col: usize, val: SqlXml) {
        self.capture(col, ColumnValues::Xml(val));
    }
    fn write_json(&mut self, col: usize, val: SqlJson) {
        self.capture(col, ColumnValues::Json(val));
    }
    fn write_vector(&mut self, col: usize, val: SqlVector) {
        self.capture(col, ColumnValues::Vector(val));
    }

    fn end_row(&mut self) {
        self.end_row_fired = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mssql_tds::datatypes::sql_string::SqlString;

    #[test]
    fn pauses_before_first_column_only_in_position_mode() {
        // Position/drain mode (no column requested) → true.
        let w = OdbcRowWriter::new();
        assert!(w.pause_before_first_column());
        // Resume mode (column requested) → false.
        let mut w2 = OdbcRowWriter::new();
        w2.request(0);
        assert!(!w2.pause_before_first_column());
    }

    #[test]
    fn position_mode_captures_nothing_and_never_pauses_after() {
        let mut w = OdbcRowWriter::new();
        w.write_i32(0, 10);
        w.write_string(1, SqlString::from_utf8_string("x".to_string()));
        assert!(!w.pause_after_column(0));
        assert!(!w.pause_after_column(1));
        assert!(w.take_captured().is_none());
    }

    #[test]
    fn requested_column_is_captured_and_pauses_after_it() {
        let mut w = OdbcRowWriter::new();
        w.request(1);
        assert!(!w.pause_after_column(0));
        assert!(w.pause_after_column(1));
        w.write_i32(0, 99);
        w.write_string(1, SqlString::from_utf8_string("hello".to_string()));
        match w.take_captured() {
            Some(ColumnValues::String(s)) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("expected String, got {other:?}"),
        }
        assert!(w.take_captured().is_none());
    }

    #[test]
    fn requested_null_column_is_captured() {
        let mut w = OdbcRowWriter::new();
        w.request(0);
        w.write_null(0);
        assert!(matches!(w.take_captured(), Some(ColumnValues::Null)));
    }

    #[test]
    fn end_row_flag_tracks_completion() {
        let mut w = OdbcRowWriter::new();
        w.request(0);
        assert!(!w.end_row_fired());
        w.end_row();
        assert!(w.end_row_fired());
        w.request(1);
        assert!(!w.end_row_fired());
    }

    #[test]
    fn request_resets_prior_capture() {
        let mut w = OdbcRowWriter::new();
        w.request(0);
        w.write_i32(0, 7);
        w.request(1);
        assert!(w.take_captured().is_none());
    }
}

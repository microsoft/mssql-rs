// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_string::SqlString;
use crate::datatypes::sql_vector::SqlVector;
use crate::query::metadata::ColumnMetadata;
use crate::token::tokens::SqlCollation;
use crate::core::TdsResult;
use uuid::Uuid;

/// Streaming sink for PLP column chunks.
///
/// When the decoder encounters a PLP column, it calls `RowWriter::begin_plp()`
/// to obtain a sink. The decoder then pushes data chunks through `write_chunk()`.
/// Once all chunks are written, the sink is finalized to produce a `ColumnValues`.
pub trait PlpStreamingSink: Send {
    /// Writes the next chunk of PLP data.
    fn write_chunk(&mut self, data: &[u8]) -> TdsResult<()>;

    /// Finalizes the PLP stream and returns the materialized value.
    fn finalize(self: Box<Self>) -> TdsResult<ColumnValues>;

    /// Returns the collation of this PLP column, if any.
    fn collation(&self) -> Option<SqlCollation> {
        None
    }
}

/// Pluggable decode sink for TDS row data.
///
/// The decoder calls these typed methods directly during wire decoding,
/// enabling consumers (Arrow writers, N-API binary encoders, etc.) to
/// receive values without going through the intermediate `ColumnValues` enum.
pub trait RowWriter {
    /// Creates a streaming sink for the given PLP column.
    ///
    /// Called by the decoder when encountering a PLP column. Subclasses may
    /// return a buffering sink (for full-row materilization) or a streaming sink
    /// (for incremental reads). Default returns a buffering sink.
    fn begin_plp(&mut self, _col: usize, _metadata: &ColumnMetadata) -> Box<dyn PlpStreamingSink> {
        Box::new(BufferingPlpSink::new())
    }

    /// Finalizes a PLP sink and writes the resulting value to the row.
    ///
    /// Called after all chunks have been written to the sink returned by `begin_plp()`.
    /// Default implementation writes the finalized value to the row via `write_column_value()`.
    fn finalize_plp(&mut self, col: usize, sink: Box<dyn PlpStreamingSink>) -> TdsResult<()> {
        let value = sink.finalize()?;
        Ok(write_column_value(self, col, value))
    }

    /// Returns `true` to pause row decoding after reading column `col`.
    ///
    /// Used to support incremental column iteration: pause after reading a column
    /// to let the caller process it, then resume from the next column.
    fn pause_after_column(&self, _col: usize) -> bool {
        false
    }

    /// Writes a SQL `NULL` for column `col`.
    fn write_null(&mut self, col: usize);
    /// Writes a `bit` value.
    fn write_bool(&mut self, col: usize, val: bool);
    /// Writes a `tinyint` value.
    fn write_u8(&mut self, col: usize, val: u8);
    /// Writes a `smallint` value.
    fn write_i16(&mut self, col: usize, val: i16);
    /// Writes an `int` value.
    fn write_i32(&mut self, col: usize, val: i32);
    /// Writes a `bigint` value.
    fn write_i64(&mut self, col: usize, val: i64);
    /// Writes a `real` value.
    fn write_f32(&mut self, col: usize, val: f32);
    /// Writes a `float` value.
    fn write_f64(&mut self, col: usize, val: f64);
    /// Writes a character string value.
    fn write_string(&mut self, col: usize, val: SqlString);
    /// Writes a binary value.
    fn write_bytes(&mut self, col: usize, val: Vec<u8>);
    /// Writes a `decimal` value.
    fn write_decimal(&mut self, col: usize, val: DecimalParts);
    /// Writes a `numeric` value.
    fn write_numeric(&mut self, col: usize, val: DecimalParts);
    /// Writes a `date` value.
    fn write_date(&mut self, col: usize, val: SqlDate);
    /// Writes a `time` value.
    fn write_time(&mut self, col: usize, val: SqlTime);
    /// Writes a `datetime` value.
    fn write_datetime(&mut self, col: usize, val: SqlDateTime);
    /// Writes a `smalldatetime` value.
    fn write_smalldatetime(&mut self, col: usize, val: SqlSmallDateTime);
    /// Writes a `datetime2` value.
    fn write_datetime2(&mut self, col: usize, val: SqlDateTime2);
    /// Writes a `datetimeoffset` value.
    fn write_datetimeoffset(&mut self, col: usize, val: SqlDateTimeOffset);
    /// Writes a `money` value.
    fn write_money(&mut self, col: usize, val: SqlMoney);
    /// Writes a `smallmoney` value.
    fn write_smallmoney(&mut self, col: usize, val: SqlSmallMoney);
    /// Writes a `uniqueidentifier` value.
    fn write_uuid(&mut self, col: usize, val: Uuid);
    /// Writes an `xml` value.
    fn write_xml(&mut self, col: usize, val: SqlXml);
    /// Writes a `json` value.
    fn write_json(&mut self, col: usize, val: SqlJson);
    /// Writes a `vector` value.
    fn write_vector(&mut self, col: usize, val: SqlVector);
    /// Signals the end of the current row.
    fn end_row(&mut self);
}

/// Buffers PLP chunks in memory and finalizes to `ColumnValues::Bytes`.
/// Used by `DefaultRowWriter` for full-row materialization.
pub struct BufferingPlpSink {
    chunks: Vec<u8>,
}

impl BufferingPlpSink {
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
        }
    }
}

impl PlpStreamingSink for BufferingPlpSink {
    fn write_chunk(&mut self, data: &[u8]) -> TdsResult<()> {
        self.chunks.extend_from_slice(data);
        Ok(())
    }

    fn finalize(self: Box<Self>) -> TdsResult<ColumnValues> {
        Ok(ColumnValues::Bytes(self.chunks))
    }
}

impl Default for BufferingPlpSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Default implementation that assembles `Vec<ColumnValues>`, preserving
/// the current decoder behavior. Existing `next_row()` callers see no change.
pub struct DefaultRowWriter {
    row: Vec<ColumnValues>,
}

impl DefaultRowWriter {
    /// Creates a writer pre-allocated for `col_count` columns.
    pub fn new(col_count: usize) -> Self {
        Self {
            row: Vec::with_capacity(col_count),
        }
    }

    /// Takes the completed row, leaving the writer ready for reuse.
    pub fn take_row(&mut self) -> Vec<ColumnValues> {
        std::mem::take(&mut self.row)
    }
}

impl RowWriter for DefaultRowWriter {
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
        // No-op for DefaultRowWriter — row is taken via take_row().
    }
}

/// Bridges a `ColumnValues` into a `RowWriter` call. Used as a fallback path
/// when the decoder has already produced a `ColumnValues` (e.g. for rare types)
/// and needs to forward it through a writer.
pub fn write_column_value<W: RowWriter + ?Sized>(writer: &mut W, col: usize, value: ColumnValues) {
    match value {
        ColumnValues::Null => writer.write_null(col),
        ColumnValues::Bit(v) => writer.write_bool(col, v),
        ColumnValues::TinyInt(v) => writer.write_u8(col, v),
        ColumnValues::SmallInt(v) => writer.write_i16(col, v),
        ColumnValues::Int(v) => writer.write_i32(col, v),
        ColumnValues::BigInt(v) => writer.write_i64(col, v),
        ColumnValues::Real(v) => writer.write_f32(col, v),
        ColumnValues::Float(v) => writer.write_f64(col, v),
        ColumnValues::String(v) => writer.write_string(col, v),
        ColumnValues::Bytes(v) => writer.write_bytes(col, v),
        ColumnValues::Decimal(v) => writer.write_decimal(col, v),
        ColumnValues::Numeric(v) => writer.write_numeric(col, v),
        ColumnValues::Date(v) => writer.write_date(col, v),
        ColumnValues::Time(v) => writer.write_time(col, v),
        ColumnValues::DateTime(v) => writer.write_datetime(col, v),
        ColumnValues::SmallDateTime(v) => writer.write_smalldatetime(col, v),
        ColumnValues::DateTime2(v) => writer.write_datetime2(col, v),
        ColumnValues::DateTimeOffset(v) => writer.write_datetimeoffset(col, v),
        ColumnValues::Money(v) => writer.write_money(col, v),
        ColumnValues::SmallMoney(v) => writer.write_smallmoney(col, v),
        ColumnValues::Uuid(v) => writer.write_uuid(col, v),
        ColumnValues::Xml(v) => writer.write_xml(col, v),
        ColumnValues::Json(v) => writer.write_json(col, v),
        ColumnValues::Vector(v) => writer.write_vector(col, v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::sql_string::EncodingType;

    #[test]
    fn default_row_writer_assembles_column_values() {
        let mut writer = DefaultRowWriter::new(5);

        writer.write_i32(0, 42);
        writer.write_null(1);
        writer.write_bool(2, true);
        writer.write_f64(3, 99.5);
        writer.write_string(4, SqlString::new(b"hello".to_vec(), EncodingType::Utf16));
        writer.end_row();

        let row = writer.take_row();
        assert_eq!(row.len(), 5);
        assert_eq!(row[0], ColumnValues::Int(42));
        assert_eq!(row[1], ColumnValues::Null);
        assert_eq!(row[2], ColumnValues::Bit(true));
        assert_eq!(row[3], ColumnValues::Float(99.5));
        assert!(matches!(row[4], ColumnValues::String(_)));
    }

    #[test]
    fn default_row_writer_take_row_resets() {
        let mut writer = DefaultRowWriter::new(2);
        writer.write_i32(0, 1);
        writer.write_i32(1, 2);
        let row1 = writer.take_row();
        assert_eq!(row1.len(), 2);

        // After take, writer is empty and reusable
        writer.write_i64(0, 100);
        let row2 = writer.take_row();
        assert_eq!(row2.len(), 1);
        assert_eq!(row2[0], ColumnValues::BigInt(100));
    }

    #[test]
    fn write_column_value_bridges_all_types() {
        let mut writer = DefaultRowWriter::new(3);

        write_column_value(&mut writer, 0, ColumnValues::Int(99));
        write_column_value(&mut writer, 1, ColumnValues::Null);
        write_column_value(&mut writer, 2, ColumnValues::Bit(false));

        let row = writer.take_row();
        assert_eq!(row[0], ColumnValues::Int(99));
        assert_eq!(row[1], ColumnValues::Null);
        assert_eq!(row[2], ColumnValues::Bit(false));
    }

    #[test]
    fn write_column_value_bridges_numeric() {
        let mut writer = DefaultRowWriter::new(1);
        let parts = DecimalParts::from_i64(12345, 5, 0).unwrap();
        write_column_value(&mut writer, 0, ColumnValues::Numeric(parts.clone()));
        let row = writer.take_row();
        assert_eq!(row[0], ColumnValues::Numeric(parts));
    }

    #[test]
    fn write_column_value_bridges_temporal_types() {
        let mut writer = DefaultRowWriter::new(4);

        let date = SqlDate::create(100).unwrap();
        write_column_value(&mut writer, 0, ColumnValues::Date(date.clone()));

        let time = SqlTime {
            time_nanoseconds: 123456789,
            scale: 7,
        };
        write_column_value(&mut writer, 1, ColumnValues::Time(time.clone()));

        let dt2 = SqlDateTime2 {
            days: 50000,
            time: SqlTime {
                time_nanoseconds: 0,
                scale: 0,
            },
        };
        write_column_value(&mut writer, 2, ColumnValues::DateTime2(dt2.clone()));

        let dto = SqlDateTimeOffset {
            datetime2: dt2.clone(),
            offset: -300,
        };
        write_column_value(&mut writer, 3, ColumnValues::DateTimeOffset(dto.clone()));

        let row = writer.take_row();
        assert_eq!(row[0], ColumnValues::Date(date));
        assert_eq!(row[1], ColumnValues::Time(time));
        assert_eq!(row[2], ColumnValues::DateTime2(dt2));
        assert_eq!(row[3], ColumnValues::DateTimeOffset(dto));
    }

    #[test]
    fn write_column_value_bridges_money_types() {
        let mut writer = DefaultRowWriter::new(2);

        let money = SqlMoney::from((100, 200));
        write_column_value(&mut writer, 0, ColumnValues::Money(money.clone()));

        let small_money = SqlSmallMoney::from(42);
        write_column_value(
            &mut writer,
            1,
            ColumnValues::SmallMoney(small_money.clone()),
        );

        let row = writer.take_row();
        assert_eq!(row[0], ColumnValues::Money(money));
        assert_eq!(row[1], ColumnValues::SmallMoney(small_money));
    }

    #[test]
    fn write_all_primitive_types() {
        let mut writer = DefaultRowWriter::new(8);

        writer.write_u8(0, 255);
        writer.write_i16(1, -1000);
        writer.write_i32(2, 42);
        writer.write_i64(3, i64::MAX);
        writer.write_f32(4, 1.5);
        writer.write_f64(5, 2.5);
        writer.write_bool(6, false);
        writer.write_null(7);

        let row = writer.take_row();
        assert_eq!(row[0], ColumnValues::TinyInt(255));
        assert_eq!(row[1], ColumnValues::SmallInt(-1000));
        assert_eq!(row[2], ColumnValues::Int(42));
        assert_eq!(row[3], ColumnValues::BigInt(i64::MAX));
        assert_eq!(row[4], ColumnValues::Real(1.5));
        assert_eq!(row[5], ColumnValues::Float(2.5));
        assert_eq!(row[6], ColumnValues::Bit(false));
        assert_eq!(row[7], ColumnValues::Null);
    }
}

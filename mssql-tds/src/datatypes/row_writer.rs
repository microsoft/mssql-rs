// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::borrow::Cow;
use std::mem::MaybeUninit;

use crate::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_string::{EncodingType, SqlString};
use crate::datatypes::sql_vector::SqlVector;
use crate::datatypes::sqldatatypes::TdsDataType;
use uuid::Uuid;

/// Pluggable decode sink for TDS row data.
///
/// The decoder calls these typed methods directly during wire decoding,
/// enabling consumers (Arrow writers, N-API binary encoders, etc.) to
/// receive values without going through the intermediate `ColumnValues` enum.
pub trait RowWriter {
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
    /// Writes a character string value in its wire encoding.
    ///
    /// `bytes` borrows the read buffer when the value was already resident in
    /// it, so it must not be retained past the call. Use
    /// [`EncodingType::encoding`] to transcode it in place, or `into_owned` to
    /// take it — which is free when the decoder already owned the bytes.
    fn write_string(&mut self, col: usize, bytes: Cow<'_, [u8]>, encoding_type: EncodingType);
    /// Writes a binary value. Borrows on the same terms as
    /// [`RowWriter::write_string`].
    fn write_bytes(&mut self, col: usize, bytes: Cow<'_, [u8]>);
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
    /// Reports the base type a `sql_variant` column carries, immediately before
    /// the value write. Defaulted, so writers that do not surface the variant's
    /// underlying type are unaffected.
    fn write_variant_base_type(&mut self, _col: usize, _base: TdsDataType) {}
    /// Signals the end of the current row.
    fn end_row(&mut self);

    /// Offers the writer the chance to supply the final storage for a
    /// known-length PLP string or binary value, so the decoder reads the
    /// payload straight from the wire into it.
    ///
    /// This is the sink half of the trait. It exists for consumers that own a
    /// destination buffer already — an Arrow builder, an N-API byte encoder, or
    /// another arena-backed consumer — and lets them
    /// take the payload without `mssql-tds` allocating a `Vec` per value that
    /// the consumer then copies out of and drops.
    ///
    /// Returning `None` is the default and leaves the value on the owned
    /// [`Self::write_bytes`] / [`Self::write_string`] path, so writers that do
    /// not opt in are unaffected.
    ///
    /// # Which values are offered
    ///
    /// Only the `MAX` types, and only when the server frames them with a known
    /// total length. `USHORTLEN` values (`varchar(n)`, `varbinary(n)`), the
    /// legacy `TEXT`/`NTEXT`/`IMAGE` `LONGLEN` types, `PLP_UNKNOWNLEN` streams
    /// and NULLs all stay on the owned path unconditionally.
    ///
    /// Short values remain on the existing hot path so writers that decline
    /// destinations do not pay for an extra branch on every small payload.
    /// PLP values are the useful boundary because they can be large or numerous
    /// and already require chunked decoding.
    ///
    /// # Contract
    ///
    /// A writer that returns `Some` must return a slice of exactly `length`
    /// bytes and receives exactly one matching [`Self::commit_value`] call for
    /// the same `col`. It does not additionally receive `write_bytes` or
    /// `write_string` for that value.
    ///
    /// The destination may contain uninitialized bytes. When `commit_value`
    /// receives `complete: true`, every element has been initialized and the
    /// writer may soundly treat the destination as bytes. When it receives
    /// `false`, the writer must discard the destination without reading it.
    ///
    /// `length` counts bytes as framed on the wire, not characters. Raw wire
    /// bytes are handed over as-is together with their [`ValueKind`], so a
    /// consumer that transcodes downstream never pays for a transcode here.
    /// A writer whose storage cannot hold the wire form — because it needs to
    /// transcode from, say, [`EncodingType::Utf16`] first — returns `None` for
    /// that value and receives it through [`Self::write_string`] as before.
    fn value_destination<'a>(
        &'a mut self,
        _col: usize,
        _kind: ValueKind<'_>,
        _length: usize,
    ) -> Option<&'a mut [MaybeUninit<u8>]> {
        None
    }

    /// Completes a value whose storage came from [`Self::value_destination`].
    ///
    /// `complete` is `false` when decoding failed partway through; the destination
    /// may then contain uninitialized bytes, so the writer must discard it without
    /// reading it.
    ///
    /// # Cancellation
    ///
    /// Errors that return through the transport — operation timeout, an
    /// explicit cancel, a malformed token — reach this method with `complete`
    /// set to `false`. Dropping the row-decode future outright does not: no
    /// further decoder code runs, so an offered destination is left
    /// uncommitted. An RAII guard cannot close that gap here, because the guard
    /// and the destination slice would both have to borrow the writer at once.
    ///
    /// A writer must therefore treat a destination that is still pending at the
    /// next [`Self::end_row`], or at the next `value_destination` for the same
    /// column, as abandoned rather than asserting that it was committed. The
    /// row it belonged to is not delivered in that case.
    fn commit_value(&mut self, _col: usize, _complete: bool) {}
}

/// The kind of value a [`RowWriter::value_destination`] request is for.
#[derive(Debug)]
#[non_exhaustive]
pub enum ValueKind<'a> {
    /// A binary value, handed over verbatim.
    Bytes,
    /// A character value, handed over as raw wire bytes in this encoding.
    String(&'a EncodingType),
}

/// Default implementation that assembles `Vec<ColumnValues>`, preserving
/// the current decoder behavior. Existing `next_row()` callers see no change.
pub struct DefaultRowWriter {
    row: Vec<ColumnValues>,
    /// Base type of each `sql_variant` value, keyed by its position in `row`.
    /// Empty unless the row contained a variant column.
    variant_bases: Vec<(usize, TdsDataType)>,
}

impl DefaultRowWriter {
    /// Creates a writer pre-allocated for `col_count` columns.
    pub fn new(col_count: usize) -> Self {
        Self {
            row: Vec::with_capacity(col_count),
            variant_bases: Vec::new(),
        }
    }

    /// Base type of the `sql_variant` value at `index`, or `None` when that
    /// column was not a variant. Valid until [`Self::take_row`].
    pub fn variant_base(&self, index: usize) -> Option<TdsDataType> {
        self.variant_bases
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, base)| *base)
    }

    /// Takes the completed row, leaving the writer ready for reuse.
    pub fn take_row(&mut self) -> Vec<ColumnValues> {
        self.variant_bases.clear();
        std::mem::take(&mut self.row)
    }
}

impl RowWriter for DefaultRowWriter {
    fn write_null(&mut self, _col: usize) {
        self.row.push(ColumnValues::Null);
    }

    // The hook fires before the value is pushed, so `row.len()` is the index the
    // value is about to occupy.
    fn write_variant_base_type(&mut self, _col: usize, base: TdsDataType) {
        self.variant_bases.push((self.row.len(), base));
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

    fn write_string(&mut self, _col: usize, bytes: Cow<'_, [u8]>, encoding_type: EncodingType) {
        self.row.push(ColumnValues::String(SqlString::new(
            bytes.into_owned(),
            encoding_type,
        )));
    }

    fn write_bytes(&mut self, _col: usize, bytes: Cow<'_, [u8]>) {
        self.row.push(ColumnValues::Bytes(bytes.into_owned()));
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

/// A `RowWriter` that discards every value it receives.
///
/// Used by the decode driver's *skip* path (drain-to-end and skip-to-column):
/// the wire bytes still have to be consumed so the stream stays aligned, but no
/// `ColumnValues`, `String`, or `Vec` is retained. Fixed-width types allocate
/// nothing at all; the transient value a variable-length decoder builds is
/// dropped immediately instead of being pushed onto a row `Vec`.
pub struct DiscardRowWriter;

impl RowWriter for DiscardRowWriter {
    fn write_null(&mut self, _col: usize) {}
    fn write_bool(&mut self, _col: usize, _val: bool) {}
    fn write_u8(&mut self, _col: usize, _val: u8) {}
    fn write_i16(&mut self, _col: usize, _val: i16) {}
    fn write_i32(&mut self, _col: usize, _val: i32) {}
    fn write_i64(&mut self, _col: usize, _val: i64) {}
    fn write_f32(&mut self, _col: usize, _val: f32) {}
    fn write_f64(&mut self, _col: usize, _val: f64) {}
    // Takes the `Cow` unexamined, so a borrow is never promoted to an owned
    // buffer purely to drop it.
    fn write_string(&mut self, _col: usize, _bytes: Cow<'_, [u8]>, _encoding_type: EncodingType) {}
    fn write_bytes(&mut self, _col: usize, _bytes: Cow<'_, [u8]>) {}
    fn write_decimal(&mut self, _col: usize, _val: DecimalParts) {}
    fn write_numeric(&mut self, _col: usize, _val: DecimalParts) {}
    fn write_date(&mut self, _col: usize, _val: SqlDate) {}
    fn write_time(&mut self, _col: usize, _val: SqlTime) {}
    fn write_datetime(&mut self, _col: usize, _val: SqlDateTime) {}
    fn write_smalldatetime(&mut self, _col: usize, _val: SqlSmallDateTime) {}
    fn write_datetime2(&mut self, _col: usize, _val: SqlDateTime2) {}
    fn write_datetimeoffset(&mut self, _col: usize, _val: SqlDateTimeOffset) {}
    fn write_money(&mut self, _col: usize, _val: SqlMoney) {}
    fn write_smallmoney(&mut self, _col: usize, _val: SqlSmallMoney) {}
    fn write_uuid(&mut self, _col: usize, _val: Uuid) {}
    fn write_xml(&mut self, _col: usize, _val: SqlXml) {}
    fn write_json(&mut self, _col: usize, _val: SqlJson) {}
    fn write_vector(&mut self, _col: usize, _val: SqlVector) {}
    fn end_row(&mut self) {}
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
        ColumnValues::String(v) => {
            let (bytes, encoding_type) = v.into_parts();
            writer.write_string(col, Cow::Owned(bytes), encoding_type)
        }
        ColumnValues::Bytes(v) => writer.write_bytes(col, Cow::Owned(v)),
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
    fn discard_row_writer_drops_borrowed_values() {
        let mut writer = DiscardRowWriter;
        writer.write_string(0, Cow::Borrowed(b"h\0i\0"), EncodingType::Utf16);
        writer.write_bytes(1, Cow::Borrowed(&[1, 2, 3]));
        writer.end_row();
    }

    #[test]
    fn default_row_writer_borrowed_string_matches_owned() {
        let bytes = b"h\0i\0";

        let mut borrowed = DefaultRowWriter::new(1);
        borrowed.write_string(0, Cow::Borrowed(bytes), EncodingType::Utf16);

        let mut owned = DefaultRowWriter::new(1);
        owned.write_string(0, Cow::Owned(bytes.to_vec()), EncodingType::Utf16);

        assert_eq!(borrowed.take_row(), owned.take_row());
    }

    #[test]
    fn default_row_writer_borrowed_bytes_matches_owned() {
        let mut borrowed = DefaultRowWriter::new(1);
        borrowed.write_bytes(0, Cow::Borrowed(&[9, 8, 7]));

        let mut owned = DefaultRowWriter::new(1);
        owned.write_bytes(0, Cow::Owned(vec![9, 8, 7]));

        assert_eq!(borrowed.take_row(), owned.take_row());
    }

    #[test]
    fn default_row_writer_assembles_column_values() {
        let mut writer = DefaultRowWriter::new(5);

        writer.write_i32(0, 42);
        writer.write_null(1);
        writer.write_bool(2, true);
        writer.write_f64(3, 99.5);
        writer.write_string(4, Cow::Borrowed(b"hello"), EncodingType::Utf16);
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
        write_column_value(&mut writer, 0, ColumnValues::Numeric(parts));
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

    /// A variant's base type is keyed to the position the value lands in, so a
    /// row mixing variant and non-variant columns reports the right base for
    /// each, and nothing for the others.
    #[test]
    fn default_row_writer_keys_variant_base_types_to_their_column() {
        let mut writer = DefaultRowWriter::new(3);

        writer.write_i32(0, 1);
        writer.write_variant_base_type(1, TdsDataType::NVarChar);
        writer.write_string(1, Cow::Borrowed(&[0x41, 0x00]), EncodingType::Utf16);
        writer.write_variant_base_type(2, TdsDataType::Int4);
        writer.write_i32(2, 7);
        writer.end_row();

        assert_eq!(writer.variant_base(0), None);
        assert_eq!(writer.variant_base(1), Some(TdsDataType::NVarChar));
        assert_eq!(writer.variant_base(2), Some(TdsDataType::Int4));
        // Out of range is simply "not a variant".
        assert_eq!(writer.variant_base(9), None);

        // Taking the row clears the bases so the writer can be reused.
        assert_eq!(writer.take_row().len(), 3);
        assert_eq!(writer.variant_base(1), None);
    }
}

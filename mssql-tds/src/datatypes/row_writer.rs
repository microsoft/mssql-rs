// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_string::{EncodingType, SqlString};
use crate::datatypes::sql_vector::SqlVector;
use uuid::Uuid;

/// Upper bound on eager allocation from a declared length.
///
/// A PLP header may declare up to `MAX_PLP_SIZE` (~2 GiB) before a single payload byte
/// arrives. Honouring that literally lets a malicious or malfunctioning server force a
/// 2 GiB allocation per column. Preallocating up to this cap keeps the common case at
/// zero reallocations while leaving anything larger to grow on demand.
const PREALLOC_CAP: usize = 1 << 20;

/// Byte length of a value the decoder is about to stream, when it is known up front.
///
/// PLP (partially-length-prefixed) columns may declare `UNKNOWN`, in which case the
/// total length only becomes apparent when the terminating zero-length chunk arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LenHint {
    /// The decoder has already range-checked this length; it is the exact byte count.
    Exact(usize),
    /// Length is not known until the value ends.
    Unknown,
}

/// What a streamed value represents, so the writer can choose its own representation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueKind {
    /// Character data in the given wire encoding.
    Str(EncodingType),
    /// Opaque binary data.
    Blob,
}

/// Pluggable decode sink for TDS row data.
///
/// The decoder calls these typed methods directly during wire decoding,
/// enabling consumers (Arrow writers, N-API binary encoders, etc.) to
/// receive values without going through the intermediate `ColumnValues` enum.
///
/// # Streaming variable-length values
///
/// Fixed-width values arrive by value and cost nothing to pass. Character and binary
/// values are different: the decoder used to allocate a `Vec<u8>`, hand over ownership,
/// and let the writer copy it into its own layout. For a PLP `nvarchar(max)` reaching
/// the N-API encoder that meant three full-size allocations of the same payload.
///
/// [`begin_value`](RowWriter::begin_value) / [`reserve`](RowWriter::reserve) /
/// [`commit_value`](RowWriter::commit_value) invert that: the writer hands the decoder
/// somewhere to put bytes, so the payload lands in its final buffer on the first copy.
///
/// ```ignore
/// writer.begin_value(col, ValueKind::Str(EncodingType::Utf16), LenHint::Exact(n));
/// let dst = writer.reserve(col, n);      // repeat per chunk for PLP
/// reader.read_bytes(dst).await?;
/// writer.commit_value(col);             // value is complete; writer may transcode
/// ```
///
/// ## Contract
///
/// | | |
/// |---|---|
/// | Ordering | `begin_value` → `reserve`* → `commit_value`, per column |
/// | Overlap | at most one value in progress per writer |
/// | Trust | `LenHint::Exact(n)` is the true byte count |
/// | Length | `reserve(col, n)` must return exactly `n` bytes, never fewer |
/// | Errors | writers never fail; a decode failure surfaces as [`abandon_row`](RowWriter::abandon_row) |
/// | Encoding | writers receive raw wire bytes; transcoding is the writer's choice and is lossy |
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
    /// Writes a character string value from borrowed wire bytes.
    fn write_str(&mut self, col: usize, bytes: &[u8], encoding: EncodingType);
    /// Writes a binary value from borrowed wire bytes.
    fn write_blob(&mut self, col: usize, bytes: &[u8]);
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

    /// Announces a variable-length value that will be streamed in via [`reserve`](Self::reserve).
    ///
    /// `hint` lets the writer size its buffer once instead of growing per chunk. A writer
    /// may ignore it; it must not treat `Exact(n)` as permission to skip `commit_value`,
    /// because only the decoder can see a PLP terminator.
    fn begin_value(&mut self, col: usize, kind: ValueKind, hint: LenHint);

    /// Returns exactly `n` writable bytes for the value opened by [`begin_value`](Self::begin_value).
    ///
    /// `n` is the length of the chunk the decoder is about to read, never a packet
    /// remainder — TDS packet framing stays invisible to writers. Called repeatedly for
    /// chunked PLP payloads.
    ///
    /// Returning fewer than `n` bytes desyncs the stream, so implementations must panic
    /// rather than short-return. The returned slice must be initialized (`resize`, not
    /// `set_len`) because the decoder may read it back.
    fn reserve(&mut self, col: usize, n: usize) -> &mut [u8];

    /// Marks the value complete.
    ///
    /// This is the only signal that a `LenHint::Unknown` value has ended, so it is
    /// required rather than merely tidy. Writers that buffer fragments reassemble here
    /// and release them; writers that transcode do so here, lossily.
    fn commit_value(&mut self, col: usize);

    /// Signals the end of the current row.
    fn end_row(&mut self);

    /// Discards everything written for the current row.
    ///
    /// Called when decoding fails partway through a row. Without it, values already
    /// written for that row stay in the writer's buffer and shift every subsequent
    /// column, silently corrupting the result set rather than failing it.
    fn abandon_row(&mut self);
}

/// Default implementation that assembles `Vec<ColumnValues>`, preserving
/// the current decoder behavior. Existing `next_row()` callers see no change.
pub struct DefaultRowWriter {
    row: Vec<ColumnValues>,
    scratch: Vec<u8>,
    pending: Option<ValueKind>,
}

impl DefaultRowWriter {
    /// Creates a writer pre-allocated for `col_count` columns.
    pub fn new(col_count: usize) -> Self {
        Self {
            row: Vec::with_capacity(col_count),
            scratch: Vec::new(),
            pending: None,
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

    fn write_str(&mut self, _col: usize, bytes: &[u8], encoding: EncodingType) {
        self.row.push(ColumnValues::String(SqlString::new(
            bytes.to_vec(),
            encoding,
        )));
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
        // No-op for DefaultRowWriter — row is taken via take_row().
    }

    fn abandon_row(&mut self) {
        self.pending = None;
        self.scratch.clear();
        self.row.clear();
    }
}

/// A `RowWriter` that discards every value it receives.
///
/// Used by the decode driver's *skip* path (drain-to-end and skip-to-column):
/// the wire bytes still have to be consumed so the stream stays aligned, but no
/// `ColumnValues`, `String`, or `Vec` is retained. Fixed-width types allocate
/// nothing at all; the transient value a variable-length decoder builds is
/// dropped immediately instead of being pushed onto a row `Vec`.
///
/// `reserve` must hand back real writable bytes, so this type carries one scratch
/// buffer that grows to the largest chunk seen and is then reused for the life of
/// the drain. See [`RowWriter::reserve`].
#[derive(Default)]
pub struct DiscardRowWriter {
    sink: Vec<u8>,
}

impl RowWriter for DiscardRowWriter {
    fn write_null(&mut self, _col: usize) {}
    fn write_bool(&mut self, _col: usize, _val: bool) {}
    fn write_u8(&mut self, _col: usize, _val: u8) {}
    fn write_i16(&mut self, _col: usize, _val: i16) {}
    fn write_i32(&mut self, _col: usize, _val: i32) {}
    fn write_i64(&mut self, _col: usize, _val: i64) {}
    fn write_f32(&mut self, _col: usize, _val: f32) {}
    fn write_f64(&mut self, _col: usize, _val: f64) {}
    fn write_str(&mut self, _col: usize, _bytes: &[u8], _encoding: EncodingType) {}
    fn write_blob(&mut self, _col: usize, _bytes: &[u8]) {}
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

    fn begin_value(&mut self, _col: usize, _kind: ValueKind, _hint: LenHint) {}

    fn reserve(&mut self, _col: usize, n: usize) -> &mut [u8] {
        // Overwrite rather than append: discarded bytes are never read back, so one
        // buffer sized to the largest chunk serves the whole drain.
        if self.sink.len() < n {
            self.sink.resize(n, 0);
        }
        &mut self.sink[..n]
    }

    fn commit_value(&mut self, _col: usize) {}
    fn end_row(&mut self) {}
    fn abandon_row(&mut self) {}
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
        ColumnValues::String(v) => writer.write_str(col, &v.bytes, v.encoding()),
        ColumnValues::Bytes(v) => writer.write_blob(col, &v),
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
        writer.write_str(4, b"h\0e\0l\0l\0o\0", EncodingType::Utf16);
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

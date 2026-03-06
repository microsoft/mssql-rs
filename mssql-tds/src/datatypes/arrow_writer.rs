// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arrow_array::RecordBatch;
use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, FixedSizeBinaryBuilder,
    Float32Builder, Float64Builder, Int16Builder, Int32Builder, Int64Builder, StringBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder, UInt8Builder,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};

use crate::core::TdsResult;
use crate::datatypes::column_values::{
    SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
    SqlSmallMoney, SqlTime, SqlXml,
};
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::row_writer::RowWriter;
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_string::SqlString;
use crate::datatypes::sql_vector::SqlVector;
use uuid::Uuid;

use std::sync::Arc;

/// Days between 0001-01-01 (TDS epoch for DATE/DATETIME2) and 1970-01-01 (Unix epoch).
const DAYS_0001_TO_UNIX: i32 = 719_162;

/// Days between 1900-01-01 (TDS epoch for DATETIME/SMALLDATETIME) and 1970-01-01 (Unix epoch).
const DAYS_1900_TO_UNIX: i32 = 25_567;

/// Microseconds per day.
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Microseconds per minute.
const MICROS_PER_MINUTE: i64 = 60_000_000;

/// Arrow column builder, lazily initialized on the first typed write.
enum ColumnBuilder {
    Uninitialized {
        pending_nulls: usize,
    },
    Boolean(BooleanBuilder),
    UInt8(UInt8Builder),
    Int16(Int16Builder),
    Int32(Int32Builder),
    Int64(Int64Builder),
    Float32(Float32Builder),
    Float64(Float64Builder),
    Decimal128 {
        builder: Decimal128Builder,
        precision: u8,
        scale: i8,
    },
    Utf8(StringBuilder),
    Binary(BinaryBuilder),
    Date32(Date32Builder),
    Time64Microsecond(Time64MicrosecondBuilder),
    TimestampMicrosecond(TimestampMicrosecondBuilder),
    TimestampMicrosecondUtc(TimestampMicrosecondBuilder),
    FixedSizeBinary16(FixedSizeBinaryBuilder),
}

impl ColumnBuilder {
    fn append_nulls(&mut self, n: usize) {
        for _ in 0..n {
            match self {
                Self::Uninitialized { pending_nulls } => *pending_nulls += 1,
                Self::Boolean(b) => b.append_null(),
                Self::UInt8(b) => b.append_null(),
                Self::Int16(b) => b.append_null(),
                Self::Int32(b) => b.append_null(),
                Self::Int64(b) => b.append_null(),
                Self::Float32(b) => b.append_null(),
                Self::Float64(b) => b.append_null(),
                Self::Decimal128 { builder, .. } => builder.append_null(),
                Self::Utf8(b) => b.append_null(),
                Self::Binary(b) => b.append_null(),
                Self::Date32(b) => b.append_null(),
                Self::Time64Microsecond(b) => b.append_null(),
                Self::TimestampMicrosecond(b) => b.append_null(),
                Self::TimestampMicrosecondUtc(b) => b.append_null(),
                Self::FixedSizeBinary16(b) => b.append_null(),
            }
        }
    }

    fn into_field_and_array(self, name: &str) -> (Field, Arc<dyn arrow_array::Array>) {
        match self {
            Self::Uninitialized { pending_nulls } => {
                let array = arrow_array::new_null_array(&DataType::Null, pending_nulls);
                let field = Field::new(name, DataType::Null, true);
                (field, array)
            }
            Self::Boolean(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Boolean, true);
                (field, Arc::new(arr))
            }
            Self::UInt8(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::UInt8, true);
                (field, Arc::new(arr))
            }
            Self::Int16(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Int16, true);
                (field, Arc::new(arr))
            }
            Self::Int32(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Int32, true);
                (field, Arc::new(arr))
            }
            Self::Int64(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Int64, true);
                (field, Arc::new(arr))
            }
            Self::Float32(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Float32, true);
                (field, Arc::new(arr))
            }
            Self::Float64(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Float64, true);
                (field, Arc::new(arr))
            }
            Self::Decimal128 {
                mut builder,
                precision,
                scale,
            } => {
                let arr = builder.finish();
                let field = Field::new(name, DataType::Decimal128(precision, scale), true);
                (field, Arc::new(arr))
            }
            Self::Utf8(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Utf8, true);
                (field, Arc::new(arr))
            }
            Self::Binary(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Binary, true);
                (field, Arc::new(arr))
            }
            Self::Date32(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Date32, true);
                (field, Arc::new(arr))
            }
            Self::Time64Microsecond(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::Time64(TimeUnit::Microsecond), true);
                (field, Arc::new(arr))
            }
            Self::TimestampMicrosecond(mut b) => {
                let arr = b.finish();
                let field =
                    Field::new(name, DataType::Timestamp(TimeUnit::Microsecond, None), true);
                (field, Arc::new(arr))
            }
            Self::TimestampMicrosecondUtc(mut b) => {
                let arr = b.finish();
                let field = Field::new(
                    name,
                    DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into())),
                    true,
                );
                (field, Arc::new(arr))
            }
            Self::FixedSizeBinary16(mut b) => {
                let arr = b.finish();
                let field = Field::new(name, DataType::FixedSizeBinary(16), true);
                (field, Arc::new(arr))
            }
        }
    }
}

/// Initializes a typed builder, pre-filling with `pending_nulls` null entries,
/// then returns the builder variant via the provided closure.
macro_rules! init_builder {
    ($col:expr, $pending:expr, $variant:ident, $builder_ty:ident) => {{
        let mut b = $builder_ty::new();
        for _ in 0..$pending {
            b.append_null();
        }
        *$col = ColumnBuilder::$variant(b);
        $col
    }};
}

/// RowWriter → Arrow RecordBatch.
///
/// Lazily discovers column types from the first non-null write to each column.
/// Call [`finish`](Self::finish) after all rows have been written.
pub(crate) struct ArrowRowWriter {
    columns: Vec<ColumnBuilder>,
    names: Vec<String>,
}

impl ArrowRowWriter {
    fn new(names: Vec<String>) -> Self {
        let col_count = names.len();
        Self {
            columns: (0..col_count)
                .map(|_| ColumnBuilder::Uninitialized { pending_nulls: 0 })
                .collect(),
            names,
        }
    }

    /// Consumes the writer and produces a `RecordBatch`.
    fn finish(self) -> TdsResult<RecordBatch> {
        let mut fields = Vec::with_capacity(self.columns.len());
        let mut arrays: Vec<Arc<dyn arrow_array::Array>> = Vec::with_capacity(self.columns.len());

        for (col, name) in self.columns.into_iter().zip(self.names.iter()) {
            let (field, array) = col.into_field_and_array(name);
            fields.push(field);
            arrays.push(array);
        }

        let schema = Arc::new(Schema::new(fields));
        RecordBatch::try_new(schema, arrays).map_err(|e| {
            crate::error::Error::TypeConversionError(format!("Arrow RecordBatch error: {e}"))
        })
    }

    /// Ensures the column at `col` is initialized to the given builder type.
    /// Returns a mutable reference to the column builder.
    fn ensure_builder(&mut self, col: usize) -> &mut ColumnBuilder {
        &mut self.columns[col]
    }
}

/// Converts `DecimalParts` into an `i128` value for Arrow Decimal128.
fn decimal_parts_to_i128(parts: &DecimalParts) -> i128 {
    let mut value: i128 = 0;
    for (i, &part) in parts.int_parts.iter().enumerate() {
        value |= (part as u32 as i128) << (i * 32);
    }
    if !parts.is_positive {
        value = -value;
    }
    value
}

/// Converts TDS date (days since 0001-01-01) to Arrow Date32 (days since 1970-01-01).
fn tds_date_to_arrow_date32(days_since_0001: u32) -> i32 {
    days_since_0001 as i32 - DAYS_0001_TO_UNIX
}

/// Converts SqlTime to microseconds since midnight.
fn sql_time_to_micros(time: &SqlTime) -> i64 {
    (time.time_nanoseconds / 1000) as i64
}

/// Converts TDS DATETIME2 (days since 0001-01-01 + time) to microseconds since Unix epoch.
fn datetime2_to_epoch_micros(days: u32, time: &SqlTime) -> i64 {
    let day_offset = days as i64 - DAYS_0001_TO_UNIX as i64;
    day_offset * MICROS_PER_DAY + sql_time_to_micros(time)
}

/// Converts TDS DATETIME (days since 1900-01-01 + 1/300s ticks) to microseconds since Unix epoch.
fn datetime_to_epoch_micros(days: i32, ticks_300: u32) -> i64 {
    let day_offset = days as i64 - DAYS_1900_TO_UNIX as i64;
    // 1 tick = 1/300 second = 3_333.333… μs
    let time_micros = (ticks_300 as i64 * 1_000_000) / 300;
    day_offset * MICROS_PER_DAY + time_micros
}

/// Converts TDS SMALLDATETIME (days since 1900-01-01 + minutes) to microseconds since Unix epoch.
fn smalldatetime_to_epoch_micros(days: u16, minutes: u16) -> i64 {
    let day_offset = days as i64 - DAYS_1900_TO_UNIX as i64;
    day_offset * MICROS_PER_DAY + minutes as i64 * MICROS_PER_MINUTE
}

/// Converts SqlMoney (mixed-endian 8-byte integer / 10^4) to f64.
fn sql_money_to_f64(money: &SqlMoney) -> f64 {
    let lsb_in_i64 = (money.lsb_part as i64) & 0x00000000FFFFFFFF;
    let raw = lsb_in_i64 | ((money.msb_part as i64) << 32);
    raw as f64 / 10_000.0
}

impl RowWriter for ArrowRowWriter {
    fn write_null(&mut self, col: usize) {
        self.columns[col].append_nulls(1);
    }

    fn write_bool(&mut self, col: usize, val: bool) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Boolean, BooleanBuilder);
        }
        if let ColumnBuilder::Boolean(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_u8(&mut self, col: usize, val: u8) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, UInt8, UInt8Builder);
        }
        if let ColumnBuilder::UInt8(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_i16(&mut self, col: usize, val: i16) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Int16, Int16Builder);
        }
        if let ColumnBuilder::Int16(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_i32(&mut self, col: usize, val: i32) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Int32, Int32Builder);
        }
        if let ColumnBuilder::Int32(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_i64(&mut self, col: usize, val: i64) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Int64, Int64Builder);
        }
        if let ColumnBuilder::Int64(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_f32(&mut self, col: usize, val: f32) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Float32, Float32Builder);
        }
        if let ColumnBuilder::Float32(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_f64(&mut self, col: usize, val: f64) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Float64, Float64Builder);
        }
        if let ColumnBuilder::Float64(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_string(&mut self, col: usize, val: SqlString) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Utf8, StringBuilder);
        }
        if let ColumnBuilder::Utf8(b) = &mut self.columns[col] {
            b.append_value(val.to_string());
        }
    }

    fn write_bytes(&mut self, col: usize, val: Vec<u8>) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Binary, BinaryBuilder);
        }
        if let ColumnBuilder::Binary(b) = &mut self.columns[col] {
            b.append_value(&val);
        }
    }

    fn write_decimal(&mut self, col: usize, val: DecimalParts) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            let mut builder = Decimal128Builder::new()
                .with_precision_and_scale(val.precision, val.scale as i8)
                .unwrap();
            for _ in 0..n {
                builder.append_null();
            }
            *slot = ColumnBuilder::Decimal128 {
                builder,
                precision: val.precision,
                scale: val.scale as i8,
            };
        }
        if let ColumnBuilder::Decimal128 { builder, .. } = &mut self.columns[col] {
            builder.append_value(decimal_parts_to_i128(&val));
        }
    }

    fn write_numeric(&mut self, col: usize, val: DecimalParts) {
        self.write_decimal(col, val);
    }

    fn write_date(&mut self, col: usize, val: SqlDate) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Date32, Date32Builder);
        }
        if let ColumnBuilder::Date32(b) = &mut self.columns[col] {
            b.append_value(tds_date_to_arrow_date32(val.get_days()));
        }
    }

    fn write_time(&mut self, col: usize, val: SqlTime) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Time64Microsecond, Time64MicrosecondBuilder);
        }
        if let ColumnBuilder::Time64Microsecond(b) = &mut self.columns[col] {
            b.append_value(sql_time_to_micros(&val));
        }
    }

    fn write_datetime(&mut self, col: usize, val: SqlDateTime) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, TimestampMicrosecond, TimestampMicrosecondBuilder);
        }
        if let ColumnBuilder::TimestampMicrosecond(b) = &mut self.columns[col] {
            b.append_value(datetime_to_epoch_micros(val.days, val.time));
        }
    }

    fn write_smalldatetime(&mut self, col: usize, val: SqlSmallDateTime) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, TimestampMicrosecond, TimestampMicrosecondBuilder);
        }
        if let ColumnBuilder::TimestampMicrosecond(b) = &mut self.columns[col] {
            b.append_value(smalldatetime_to_epoch_micros(val.days, val.time));
        }
    }

    fn write_datetime2(&mut self, col: usize, val: SqlDateTime2) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, TimestampMicrosecond, TimestampMicrosecondBuilder);
        }
        if let ColumnBuilder::TimestampMicrosecond(b) = &mut self.columns[col] {
            b.append_value(datetime2_to_epoch_micros(val.days, &val.time));
        }
    }

    fn write_datetimeoffset(&mut self, col: usize, val: SqlDateTimeOffset) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            let mut b = TimestampMicrosecondBuilder::new().with_timezone("+00:00");
            for _ in 0..n {
                b.append_null();
            }
            *slot = ColumnBuilder::TimestampMicrosecondUtc(b);
        }
        if let ColumnBuilder::TimestampMicrosecondUtc(b) = &mut self.columns[col] {
            // Convert local time to UTC by subtracting the offset.
            let local_micros = datetime2_to_epoch_micros(val.datetime2.days, &val.datetime2.time);
            let offset_micros = val.offset as i64 * MICROS_PER_MINUTE;
            b.append_value(local_micros - offset_micros);
        }
    }

    fn write_money(&mut self, col: usize, val: SqlMoney) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Float64, Float64Builder);
        }
        if let ColumnBuilder::Float64(b) = &mut self.columns[col] {
            b.append_value(sql_money_to_f64(&val));
        }
    }

    fn write_smallmoney(&mut self, col: usize, val: SqlSmallMoney) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Float64, Float64Builder);
        }
        if let ColumnBuilder::Float64(b) = &mut self.columns[col] {
            b.append_value(val.int_val as f64 / 10_000.0);
        }
    }

    fn write_uuid(&mut self, col: usize, val: Uuid) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            let mut b = FixedSizeBinaryBuilder::new(16);
            for _ in 0..n {
                b.append_null();
            }
            *slot = ColumnBuilder::FixedSizeBinary16(b);
        }
        if let ColumnBuilder::FixedSizeBinary16(b) = &mut self.columns[col] {
            b.append_value(val.as_bytes()).unwrap();
        }
    }

    fn write_xml(&mut self, col: usize, val: SqlXml) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Utf8, StringBuilder);
        }
        if let ColumnBuilder::Utf8(b) = &mut self.columns[col] {
            b.append_value(val.as_string());
        }
    }

    fn write_json(&mut self, col: usize, val: SqlJson) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Utf8, StringBuilder);
        }
        if let ColumnBuilder::Utf8(b) = &mut self.columns[col] {
            b.append_value(val.as_string());
        }
    }

    fn write_vector(&mut self, col: usize, val: SqlVector) {
        let slot = self.ensure_builder(col);
        if let ColumnBuilder::Uninitialized { pending_nulls } = slot {
            let n = *pending_nulls;
            init_builder!(slot, n, Utf8, StringBuilder);
        }
        if let ColumnBuilder::Utf8(b) = &mut self.columns[col] {
            b.append_value(format!("{val:?}"));
        }
    }

    fn end_row(&mut self) {
        // No buffering needed — values are appended directly to Arrow builders.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::sql_string::EncodingType;
    use arrow_array::Array;

    #[test]
    fn basic_int_columns() {
        let mut w = ArrowRowWriter::new(vec!["a".into(), "b".into()]);
        w.write_i32(0, 1);
        w.write_i64(1, 100);
        w.end_row();
        w.write_i32(0, 2);
        w.write_i64(1, 200);
        w.end_row();

        let batch = w.finish().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(*batch.schema().field(0).data_type(), DataType::Int32);
        assert_eq!(*batch.schema().field(1).data_type(), DataType::Int64);
    }

    #[test]
    fn nullable_column_with_leading_nulls() {
        let mut w = ArrowRowWriter::new(vec!["x".into()]);
        w.write_null(0);
        w.write_null(0);
        w.write_i32(0, 42);
        w.end_row();

        let batch = w.finish().unwrap();
        assert_eq!(batch.num_rows(), 3);
        let arr = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .unwrap();
        assert!(arr.is_null(0));
        assert!(arr.is_null(1));
        assert_eq!(arr.value(2), 42);
    }

    #[test]
    fn all_null_column_produces_null_array() {
        let mut w = ArrowRowWriter::new(vec!["n".into()]);
        w.write_null(0);
        w.write_null(0);
        w.end_row();

        let batch = w.finish().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(*batch.schema().field(0).data_type(), DataType::Null);
    }

    #[test]
    fn string_column() {
        let mut w = ArrowRowWriter::new(vec!["s".into()]);
        w.write_string(0, SqlString::new(b"hello".to_vec(), EncodingType::Utf16));
        w.end_row();

        let batch = w.finish().unwrap();
        assert_eq!(*batch.schema().field(0).data_type(), DataType::Utf8);
    }

    #[test]
    fn bool_and_f64_columns() {
        let mut w = ArrowRowWriter::new(vec!["flag".into(), "val".into()]);
        w.write_bool(0, true);
        w.write_f64(1, 99.5);
        w.end_row();

        let batch = w.finish().unwrap();
        assert_eq!(*batch.schema().field(0).data_type(), DataType::Boolean);
        assert_eq!(*batch.schema().field(1).data_type(), DataType::Float64);
    }

    #[test]
    fn date_and_time_columns() {
        let mut w = ArrowRowWriter::new(vec!["d".into(), "t".into()]);
        let date = SqlDate::create(DAYS_0001_TO_UNIX as u32).unwrap(); // Unix epoch
        let time = SqlTime {
            time_nanoseconds: 1_000_000, // 1ms = 1000μs
            scale: 7,
        };
        w.write_date(0, date);
        w.write_time(1, time);
        w.end_row();

        let batch = w.finish().unwrap();
        let date_arr = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::Date32Array>()
            .unwrap();
        assert_eq!(date_arr.value(0), 0); // Unix epoch → 0

        let time_arr = batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow_array::Time64MicrosecondArray>()
            .unwrap();
        assert_eq!(time_arr.value(0), 1000); // 1ms = 1000μs
    }

    #[test]
    fn uuid_column() {
        let mut w = ArrowRowWriter::new(vec!["id".into()]);
        let uuid = Uuid::nil();
        w.write_uuid(0, uuid);
        w.end_row();

        let batch = w.finish().unwrap();
        assert_eq!(
            *batch.schema().field(0).data_type(),
            DataType::FixedSizeBinary(16)
        );
    }

    #[test]
    fn decimal_column() {
        let mut w = ArrowRowWriter::new(vec!["d".into()]);
        let parts = DecimalParts::from_i64(12345, 10, 2).unwrap();
        w.write_decimal(0, parts);
        w.end_row();

        let batch = w.finish().unwrap();
        assert!(matches!(
            batch.schema().field(0).data_type(),
            DataType::Decimal128(10, 2)
        ));
    }

    #[test]
    fn datetime2_to_timestamp() {
        let mut w = ArrowRowWriter::new(vec!["ts".into()]);
        // 2000-01-01 00:00:00 UTC → days since 0001-01-01 = 730_119
        let dt2 = SqlDateTime2 {
            days: 730_119,
            time: SqlTime {
                time_nanoseconds: 0,
                scale: 7,
            },
        };
        w.write_datetime2(0, dt2);
        w.end_row();

        let batch = w.finish().unwrap();
        assert_eq!(
            *batch.schema().field(0).data_type(),
            DataType::Timestamp(TimeUnit::Microsecond, None)
        );
        let arr = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::TimestampMicrosecondArray>()
            .unwrap();
        // 2000-01-01 = 10_957 days since epoch → 10957 * 86400 * 1_000_000 μs
        let expected = 10_957i64 * MICROS_PER_DAY;
        assert_eq!(arr.value(0), expected);
    }

    #[test]
    fn datetimeoffset_converts_to_utc() {
        let mut w = ArrowRowWriter::new(vec!["ts".into()]);
        // 2000-01-01 05:30:00 +05:30 → should be 2000-01-01 00:00:00 UTC
        let dt2 = SqlDateTime2 {
            days: 730_119,
            time: SqlTime {
                time_nanoseconds: 5 * 3_600_000_000_000u64 + 30 * 60_000_000_000, // 5h30m
                scale: 7,
            },
        };
        let dto = SqlDateTimeOffset {
            datetime2: dt2,
            offset: 330, // +5:30 in minutes
        };
        w.write_datetimeoffset(0, dto);
        w.end_row();

        let batch = w.finish().unwrap();
        assert_eq!(
            *batch.schema().field(0).data_type(),
            DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into()))
        );
        let arr = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::TimestampMicrosecondArray>()
            .unwrap();
        let expected = 10_957i64 * MICROS_PER_DAY; // midnight UTC on 2000-01-01
        assert_eq!(arr.value(0), expected);
    }

    #[test]
    fn money_column() {
        let mut w = ArrowRowWriter::new(vec!["m".into()]);
        // $1.00 → raw TDS value 10000 (value × 10^4)
        let money = SqlMoney {
            lsb_part: 10000,
            msb_part: 0,
        };
        w.write_money(0, money);
        w.end_row();

        let batch = w.finish().unwrap();
        let arr = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::Float64Array>()
            .unwrap();
        assert!((arr.value(0) - 1.0).abs() < f64::EPSILON);
    }
}

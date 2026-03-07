// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, FixedSizeBinaryBuilder,
    Float32Builder, Float64Builder, Int16Builder, Int32Builder, Int64Builder, StringBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder, UInt8Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, TimeUnit};

use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::{
    SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
    SqlSmallMoney, SqlTime, SqlXml,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::row_writer::RowWriter;
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sql_vector::SqlVector;
use mssql_tds::query::metadata::ColumnMetadata;
use uuid::Uuid;

use std::sync::Arc;

use crate::error::ArrowError;

// ── Constants ────────────────────────────────────────────────────────────

const DAYS_0001_TO_UNIX: i32 = 719_162;
const DAYS_1900_TO_UNIX: i32 = 25_567;
const MICROS_PER_DAY: i64 = 86_400_000_000;
const MICROS_PER_MINUTE: i64 = 60_000_000;

// ── ColumnBuilder ────────────────────────────────────────────────────────

enum ColumnBuilder {
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
    fn append_null(&mut self) {
        match self {
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

    fn into_field_and_array(self, name: &str, nullable: bool) -> (Field, ArrayRef) {
        match self {
            Self::Boolean(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Boolean, nullable), Arc::new(arr))
            }
            Self::UInt8(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::UInt8, nullable), Arc::new(arr))
            }
            Self::Int16(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Int16, nullable), Arc::new(arr))
            }
            Self::Int32(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Int32, nullable), Arc::new(arr))
            }
            Self::Int64(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Int64, nullable), Arc::new(arr))
            }
            Self::Float32(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Float32, nullable), Arc::new(arr))
            }
            Self::Float64(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Float64, nullable), Arc::new(arr))
            }
            Self::Decimal128 {
                mut builder,
                precision,
                scale,
            } => {
                let arr = builder.finish();
                (
                    Field::new(name, DataType::Decimal128(precision, scale), nullable),
                    Arc::new(arr),
                )
            }
            Self::Utf8(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Utf8, nullable), Arc::new(arr))
            }
            Self::Binary(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Binary, nullable), Arc::new(arr))
            }
            Self::Date32(mut b) => {
                let arr = b.finish();
                (Field::new(name, DataType::Date32, nullable), Arc::new(arr))
            }
            Self::Time64Microsecond(mut b) => {
                let arr = b.finish();
                (
                    Field::new(name, DataType::Time64(TimeUnit::Microsecond), nullable),
                    Arc::new(arr),
                )
            }
            Self::TimestampMicrosecond(mut b) => {
                let arr = b.finish();
                (
                    Field::new(
                        name,
                        DataType::Timestamp(TimeUnit::Microsecond, None),
                        nullable,
                    ),
                    Arc::new(arr),
                )
            }
            Self::TimestampMicrosecondUtc(mut b) => {
                let arr = b.finish();
                (
                    Field::new(
                        name,
                        DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into())),
                        nullable,
                    ),
                    Arc::new(arr),
                )
            }
            Self::FixedSizeBinary16(mut b) => {
                let arr = b.finish();
                (
                    Field::new(name, DataType::FixedSizeBinary(16), nullable),
                    Arc::new(arr),
                )
            }
        }
    }
}

// ── Helper functions ─────────────────────────────────────────────────────

fn decimal_parts_to_i128(parts: &DecimalParts) -> i128 {
    let mut value: i128 = 0;
    for (i, &part) in parts.int_parts.iter().enumerate() {
        value |= ((part as u32) as i128) << (i * 32);
    }
    if !parts.is_positive {
        value = -value;
    }
    value
}

fn tds_date_to_arrow_date32(date: &SqlDate) -> i32 {
    date.get_days() as i32 - DAYS_0001_TO_UNIX
}

fn sql_time_to_micros(time: &SqlTime) -> i64 {
    // time_nanoseconds is the time value in nanoseconds; convert to microseconds
    (time.time_nanoseconds / 1_000) as i64
}

fn datetime2_to_epoch_micros(dt: &SqlDateTime2) -> i64 {
    let days_since_epoch = dt.days as i64 - DAYS_0001_TO_UNIX as i64;
    let time_micros = sql_time_to_micros(&dt.time);
    days_since_epoch * MICROS_PER_DAY + time_micros
}

fn datetime_to_epoch_micros(dt: &SqlDateTime) -> i64 {
    let days_since_epoch = dt.days as i64 - DAYS_1900_TO_UNIX as i64;
    // Each tick = 1/300 second = 3333.333... microseconds
    let time_micros = (dt.time as i64 * 10_000) / 3;
    days_since_epoch * MICROS_PER_DAY + time_micros
}

fn smalldatetime_to_epoch_micros(dt: &SqlSmallDateTime) -> i64 {
    let days_since_epoch = dt.days as i64 - DAYS_1900_TO_UNIX as i64;
    let time_micros = dt.time as i64 * MICROS_PER_MINUTE;
    days_since_epoch * MICROS_PER_DAY + time_micros
}

fn sql_money_to_f64(money: &SqlMoney) -> f64 {
    // TDS MONEY: mixed-endian — lsb_part is low 4 bytes, msb_part is high 4 bytes
    let lsb_in_i64 = (money.lsb_part as i64) & 0x00000000FFFFFFFF;
    let combined = lsb_in_i64 | ((money.msb_part as i64) << 32);
    combined as f64 / 10_000.0
}

// ── ArrowQueryReader ─────────────────────────────────────────────────────

/// Accumulates SQL Server query results into Arrow RecordBatch format.
pub struct ArrowQueryReader {
    columns: Vec<ColumnBuilder>,
    names: Vec<String>,
    nullable: Vec<bool>,
    row_count: usize,
    batch_size: usize,
}

impl ArrowQueryReader {
    /// Create a reader initialized from column metadata.
    /// Column builders are typed upfront — no lazy discovery.
    pub fn from_metadata(metadata: &[ColumnMetadata], batch_size: usize) -> TdsResult<Self> {
        let mut columns = Vec::with_capacity(metadata.len());
        let mut names = Vec::with_capacity(metadata.len());
        let mut nullable = Vec::with_capacity(metadata.len());

        for col in metadata {
            names.push(col.column_name.clone());
            nullable.push(col.is_nullable());
            columns.push(column_metadata_to_builder(col)?);
        }

        Ok(Self {
            columns,
            names,
            nullable,
            row_count: 0,
            batch_size,
        })
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub fn is_batch_ready(&self) -> bool {
        self.row_count >= self.batch_size
    }

    /// Drain builders into a RecordBatch. Returns None if no rows accumulated.
    pub fn finish(&mut self) -> TdsResult<Option<RecordBatch>> {
        if self.row_count == 0 {
            return Ok(None);
        }

        let columns = std::mem::take(&mut self.columns);
        let mut fields = Vec::with_capacity(columns.len());
        let mut arrays = Vec::with_capacity(columns.len());

        for (i, builder) in columns.into_iter().enumerate() {
            let (field, array) = builder.into_field_and_array(&self.names[i], self.nullable[i]);
            fields.push(field);
            arrays.push(array);
        }

        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema, arrays).map_err(ArrowError::ArrowError)?;

        // Reset for next batch — re-create builders with same metadata
        // Since we consumed the builders, we need to create fresh ones.
        // The caller should create a new reader or we reinitialize.
        self.row_count = 0;
        self.columns = Vec::new(); // Will be empty until reinit

        Ok(Some(batch))
    }

    /// Read all rows from a result set into Vec<RecordBatch>.
    pub async fn read_result_set(
        client: &mut TdsClient,
        batch_size: usize,
    ) -> TdsResult<Vec<RecordBatch>> {
        let metadata = client.get_metadata().clone();
        if metadata.is_empty() {
            return Ok(Vec::new());
        }

        let mut reader = Self::from_metadata(&metadata, batch_size)?;
        let mut batches = Vec::new();

        while client.next_row_into(&mut reader).await? {
            if reader.is_batch_ready() {
                if let Some(batch) = reader.finish()? {
                    batches.push(batch);
                }
                // Re-create builders for next batch
                reader = Self::from_metadata(&metadata, batch_size)?;
            }
        }

        // Flush remaining rows
        if reader.row_count() > 0
            && let Some(batch) = reader.finish()?
        {
            batches.push(batch);
        }

        Ok(batches)
    }
}

// ── ColumnMetadata → ColumnBuilder ───────────────────────────────────────

fn column_metadata_to_builder(col: &ColumnMetadata) -> TdsResult<ColumnBuilder> {
    use mssql_tds::datatypes::sqldatatypes::{TdsDataType, TypeInfoVariant};

    let builder = match col.data_type {
        TdsDataType::Bit | TdsDataType::BitN => ColumnBuilder::Boolean(BooleanBuilder::new()),

        TdsDataType::Int1 => ColumnBuilder::UInt8(UInt8Builder::new()),

        TdsDataType::Int2 => ColumnBuilder::Int16(Int16Builder::new()),

        TdsDataType::Int4 => ColumnBuilder::Int32(Int32Builder::new()),

        TdsDataType::Int8 => ColumnBuilder::Int64(Int64Builder::new()),

        TdsDataType::IntN => match col.type_info.length {
            1 => ColumnBuilder::UInt8(UInt8Builder::new()),
            2 => ColumnBuilder::Int16(Int16Builder::new()),
            4 => ColumnBuilder::Int32(Int32Builder::new()),
            8 => ColumnBuilder::Int64(Int64Builder::new()),
            _ => ColumnBuilder::Int64(Int64Builder::new()),
        },

        TdsDataType::Flt4 => ColumnBuilder::Float32(Float32Builder::new()),

        TdsDataType::Flt8 => ColumnBuilder::Float64(Float64Builder::new()),

        TdsDataType::FltN => match col.type_info.length {
            4 => ColumnBuilder::Float32(Float32Builder::new()),
            _ => ColumnBuilder::Float64(Float64Builder::new()),
        },

        TdsDataType::DecimalN
        | TdsDataType::NumericN
        | TdsDataType::Decimal
        | TdsDataType::Numeric => {
            let (precision, scale) = match col.type_info.type_info_variant {
                TypeInfoVariant::VarLenPrecisionScale(_, _, p, s) => (p, s as i8),
                _ => (38, 0),
            };
            ColumnBuilder::Decimal128 {
                builder: Decimal128Builder::new()
                    .with_precision_and_scale(precision, scale)
                    .map_err(ArrowError::ArrowError)?,
                precision,
                scale,
            }
        }

        TdsDataType::Money | TdsDataType::Money4 | TdsDataType::MoneyN => {
            ColumnBuilder::Float64(Float64Builder::new())
        }

        TdsDataType::DateN => ColumnBuilder::Date32(Date32Builder::new()),

        TdsDataType::TimeN => ColumnBuilder::Time64Microsecond(Time64MicrosecondBuilder::new()),

        TdsDataType::DateTime2N => {
            ColumnBuilder::TimestampMicrosecond(TimestampMicrosecondBuilder::new())
        }

        TdsDataType::DateTimeOffsetN => {
            ColumnBuilder::TimestampMicrosecondUtc(TimestampMicrosecondBuilder::new())
        }

        TdsDataType::DateTime | TdsDataType::DateTim4 | TdsDataType::DateTimeN => {
            ColumnBuilder::TimestampMicrosecond(TimestampMicrosecondBuilder::new())
        }

        TdsDataType::NVarChar
        | TdsDataType::NChar
        | TdsDataType::BigVarChar
        | TdsDataType::BigChar
        | TdsDataType::Text
        | TdsDataType::NText
        | TdsDataType::VarChar
        | TdsDataType::Char
        | TdsDataType::Xml
        | TdsDataType::Json
        | TdsDataType::Vector => ColumnBuilder::Utf8(StringBuilder::new()),

        TdsDataType::BigVarBinary
        | TdsDataType::BigBinary
        | TdsDataType::VarBinary
        | TdsDataType::Binary
        | TdsDataType::Image => ColumnBuilder::Binary(BinaryBuilder::new()),

        TdsDataType::Guid => ColumnBuilder::FixedSizeBinary16(FixedSizeBinaryBuilder::new(16)),

        _ => {
            return Err(mssql_tds::error::Error::TypeConversionError(format!(
                "unsupported TDS type {:?} for Arrow conversion",
                col.data_type
            )));
        }
    };

    Ok(builder)
}

// ── RowWriter implementation ─────────────────────────────────────────────

impl RowWriter for ArrowQueryReader {
    fn write_null(&mut self, col: usize) {
        self.columns[col].append_null();
    }

    fn write_bool(&mut self, col: usize, val: bool) {
        if let ColumnBuilder::Boolean(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_u8(&mut self, col: usize, val: u8) {
        if let ColumnBuilder::UInt8(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_i16(&mut self, col: usize, val: i16) {
        if let ColumnBuilder::Int16(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_i32(&mut self, col: usize, val: i32) {
        if let ColumnBuilder::Int32(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_i64(&mut self, col: usize, val: i64) {
        if let ColumnBuilder::Int64(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_f32(&mut self, col: usize, val: f32) {
        if let ColumnBuilder::Float32(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_f64(&mut self, col: usize, val: f64) {
        if let ColumnBuilder::Float64(b) = &mut self.columns[col] {
            b.append_value(val);
        }
    }

    fn write_string(&mut self, col: usize, val: SqlString) {
        if let ColumnBuilder::Utf8(b) = &mut self.columns[col] {
            b.append_value(val.to_string());
        }
    }

    fn write_bytes(&mut self, col: usize, val: Vec<u8>) {
        if let ColumnBuilder::Binary(b) = &mut self.columns[col] {
            b.append_value(&val);
        }
    }

    fn write_decimal(&mut self, col: usize, val: DecimalParts) {
        if let ColumnBuilder::Decimal128 { builder, .. } = &mut self.columns[col] {
            builder.append_value(decimal_parts_to_i128(&val));
        }
    }

    fn write_numeric(&mut self, col: usize, val: DecimalParts) {
        if let ColumnBuilder::Decimal128 { builder, .. } = &mut self.columns[col] {
            builder.append_value(decimal_parts_to_i128(&val));
        }
    }

    fn write_date(&mut self, col: usize, val: SqlDate) {
        if let ColumnBuilder::Date32(b) = &mut self.columns[col] {
            b.append_value(tds_date_to_arrow_date32(&val));
        }
    }

    fn write_time(&mut self, col: usize, val: SqlTime) {
        if let ColumnBuilder::Time64Microsecond(b) = &mut self.columns[col] {
            b.append_value(sql_time_to_micros(&val));
        }
    }

    fn write_datetime(&mut self, col: usize, val: SqlDateTime) {
        if let ColumnBuilder::TimestampMicrosecond(b) = &mut self.columns[col] {
            b.append_value(datetime_to_epoch_micros(&val));
        }
    }

    fn write_smalldatetime(&mut self, col: usize, val: SqlSmallDateTime) {
        if let ColumnBuilder::TimestampMicrosecond(b) = &mut self.columns[col] {
            b.append_value(smalldatetime_to_epoch_micros(&val));
        }
    }

    fn write_datetime2(&mut self, col: usize, val: SqlDateTime2) {
        if let ColumnBuilder::TimestampMicrosecond(b) = &mut self.columns[col] {
            b.append_value(datetime2_to_epoch_micros(&val));
        }
    }

    fn write_datetimeoffset(&mut self, col: usize, val: SqlDateTimeOffset) {
        if let ColumnBuilder::TimestampMicrosecondUtc(b) = &mut self.columns[col] {
            let local_micros = datetime2_to_epoch_micros(&val.datetime2);
            let offset_micros = val.offset as i64 * MICROS_PER_MINUTE;
            b.append_value(local_micros - offset_micros);
        }
    }

    fn write_money(&mut self, col: usize, val: SqlMoney) {
        if let ColumnBuilder::Float64(b) = &mut self.columns[col] {
            b.append_value(sql_money_to_f64(&val));
        }
    }

    fn write_smallmoney(&mut self, col: usize, val: SqlSmallMoney) {
        if let ColumnBuilder::Float64(b) = &mut self.columns[col] {
            b.append_value(val.int_val as f64 / 10_000.0);
        }
    }

    fn write_uuid(&mut self, col: usize, val: Uuid) {
        if let ColumnBuilder::FixedSizeBinary16(b) = &mut self.columns[col] {
            let _ = b.append_value(val.as_bytes());
        }
    }

    fn write_xml(&mut self, col: usize, val: SqlXml) {
        if let ColumnBuilder::Utf8(b) = &mut self.columns[col] {
            b.append_value(val.as_string());
        }
    }

    fn write_json(&mut self, col: usize, val: SqlJson) {
        if let ColumnBuilder::Utf8(b) = &mut self.columns[col] {
            b.append_value(String::from_utf8_lossy(&val.bytes));
        }
    }

    fn write_vector(&mut self, col: usize, val: SqlVector) {
        if let ColumnBuilder::Utf8(b) = &mut self.columns[col] {
            b.append_value(format!("{val:?}"));
        }
    }

    fn end_row(&mut self) {
        self.row_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mssql_tds::datatypes::sqldatatypes::{
        FixedLengthTypes, TdsDataType, TypeInfo, TypeInfoVariant, VariableLengthTypes,
    };
    use mssql_tds::query::metadata::ColumnMetadata;

    fn make_col(
        name: &str,
        data_type: TdsDataType,
        type_info: TypeInfo,
        nullable: bool,
    ) -> ColumnMetadata {
        ColumnMetadata {
            user_type: 0,
            flags: if nullable { 0x01 } else { 0x00 },
            type_info,
            data_type,
            column_name: name.to_string(),
            multi_part_name: None,
        }
    }

    fn fixed_type_info(dt: TdsDataType, len: usize, ft: FixedLengthTypes) -> TypeInfo {
        TypeInfo {
            tds_type: dt,
            length: len,
            type_info_variant: TypeInfoVariant::FixedLen(ft),
        }
    }

    fn varlen_type_info(dt: TdsDataType, len: usize, vt: VariableLengthTypes) -> TypeInfo {
        TypeInfo {
            tds_type: dt,
            length: len,
            type_info_variant: TypeInfoVariant::VarLen(vt, len),
        }
    }

    // ── Schema inference tests ───────────────────────────────────────────

    #[test]
    fn from_metadata_int_columns() {
        let metadata = vec![
            make_col(
                "tiny",
                TdsDataType::Int1,
                fixed_type_info(TdsDataType::Int1, 1, FixedLengthTypes::Int1),
                false,
            ),
            make_col(
                "small",
                TdsDataType::Int2,
                fixed_type_info(TdsDataType::Int2, 2, FixedLengthTypes::Int2),
                false,
            ),
            make_col(
                "regular",
                TdsDataType::Int4,
                fixed_type_info(TdsDataType::Int4, 4, FixedLengthTypes::Int4),
                true,
            ),
            make_col(
                "big",
                TdsDataType::Int8,
                fixed_type_info(TdsDataType::Int8, 8, FixedLengthTypes::Int8),
                true,
            ),
        ];

        let reader = ArrowQueryReader::from_metadata(&metadata, 100).unwrap();
        assert_eq!(reader.names, vec!["tiny", "small", "regular", "big"]);
        assert_eq!(reader.nullable, vec![false, false, true, true]);
        assert_eq!(reader.columns.len(), 4);
    }

    #[test]
    fn from_metadata_intn_disambiguation() {
        let metadata = vec![
            make_col(
                "u8_col",
                TdsDataType::IntN,
                varlen_type_info(TdsDataType::IntN, 1, VariableLengthTypes::IntN),
                true,
            ),
            make_col(
                "i16_col",
                TdsDataType::IntN,
                varlen_type_info(TdsDataType::IntN, 2, VariableLengthTypes::IntN),
                true,
            ),
            make_col(
                "i32_col",
                TdsDataType::IntN,
                varlen_type_info(TdsDataType::IntN, 4, VariableLengthTypes::IntN),
                true,
            ),
            make_col(
                "i64_col",
                TdsDataType::IntN,
                varlen_type_info(TdsDataType::IntN, 8, VariableLengthTypes::IntN),
                true,
            ),
        ];

        let mut reader = ArrowQueryReader::from_metadata(&metadata, 100).unwrap();

        // Write one row with different int types
        reader.write_u8(0, 42);
        reader.write_i16(1, -100);
        reader.write_i32(2, 999);
        reader.write_i64(3, i64::MAX);
        reader.end_row();

        let batch = reader.finish().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 4);
        assert_eq!(*batch.schema().field(0).data_type(), DataType::UInt8);
        assert_eq!(*batch.schema().field(1).data_type(), DataType::Int16);
        assert_eq!(*batch.schema().field(2).data_type(), DataType::Int32);
        assert_eq!(*batch.schema().field(3).data_type(), DataType::Int64);
    }

    #[test]
    fn from_metadata_fltn_disambiguation() {
        let metadata = vec![
            make_col(
                "f32_col",
                TdsDataType::FltN,
                varlen_type_info(TdsDataType::FltN, 4, VariableLengthTypes::FltN),
                true,
            ),
            make_col(
                "f64_col",
                TdsDataType::FltN,
                varlen_type_info(TdsDataType::FltN, 8, VariableLengthTypes::FltN),
                true,
            ),
        ];

        let mut reader = ArrowQueryReader::from_metadata(&metadata, 100).unwrap();
        reader.write_f32(0, 1.5);
        reader.write_f64(1, 2.5);
        reader.end_row();

        let batch = reader.finish().unwrap().unwrap();
        assert_eq!(*batch.schema().field(0).data_type(), DataType::Float32);
        assert_eq!(*batch.schema().field(1).data_type(), DataType::Float64);
    }

    #[test]
    fn from_metadata_decimal() {
        let metadata = vec![make_col(
            "dec",
            TdsDataType::DecimalN,
            TypeInfo {
                tds_type: TdsDataType::DecimalN,
                length: 9,
                type_info_variant: TypeInfoVariant::VarLenPrecisionScale(
                    VariableLengthTypes::DecimalN,
                    9,
                    18,
                    4,
                ),
            },
            true,
        )];

        let mut reader = ArrowQueryReader::from_metadata(&metadata, 100).unwrap();
        reader.write_decimal(
            0,
            DecimalParts {
                is_positive: true,
                scale: 4,
                precision: 18,
                int_parts: vec![123456],
            },
        );
        reader.end_row();

        let batch = reader.finish().unwrap().unwrap();
        assert_eq!(
            *batch.schema().field(0).data_type(),
            DataType::Decimal128(18, 4)
        );
    }

    // ── RowWriter + finish tests ─────────────────────────────────────────

    #[test]
    fn write_nulls_across_types() {
        let metadata = vec![
            make_col(
                "int_col",
                TdsDataType::Int4,
                fixed_type_info(TdsDataType::Int4, 4, FixedLengthTypes::Int4),
                true,
            ),
            make_col(
                "str_col",
                TdsDataType::NVarChar,
                varlen_type_info(TdsDataType::NVarChar, 200, VariableLengthTypes::NVarChar),
                true,
            ),
            make_col(
                "flt_col",
                TdsDataType::Flt8,
                fixed_type_info(TdsDataType::Flt8, 8, FixedLengthTypes::Flt8),
                true,
            ),
        ];

        let mut reader = ArrowQueryReader::from_metadata(&metadata, 100).unwrap();
        reader.write_null(0);
        reader.write_null(1);
        reader.write_null(2);
        reader.end_row();

        let batch = reader.finish().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert!(batch.column(0).is_null(0));
        assert!(batch.column(1).is_null(0));
        assert!(batch.column(2).is_null(0));
    }

    #[test]
    fn finish_empty_returns_none() {
        let metadata = vec![make_col(
            "col",
            TdsDataType::Int4,
            fixed_type_info(TdsDataType::Int4, 4, FixedLengthTypes::Int4),
            false,
        )];

        let mut reader = ArrowQueryReader::from_metadata(&metadata, 100).unwrap();
        assert!(reader.finish().unwrap().is_none());
    }

    #[test]
    fn batch_ready_at_threshold() {
        let metadata = vec![make_col(
            "id",
            TdsDataType::Int4,
            fixed_type_info(TdsDataType::Int4, 4, FixedLengthTypes::Int4),
            false,
        )];

        let mut reader = ArrowQueryReader::from_metadata(&metadata, 3).unwrap();
        assert!(!reader.is_batch_ready());

        reader.write_i32(0, 1);
        reader.end_row();
        assert!(!reader.is_batch_ready());

        reader.write_i32(0, 2);
        reader.end_row();
        assert!(!reader.is_batch_ready());

        reader.write_i32(0, 3);
        reader.end_row();
        assert!(reader.is_batch_ready());
        assert_eq!(reader.row_count(), 3);
    }

    // ── Helper function tests ────────────────────────────────────────────

    #[test]
    fn decimal_parts_positive_and_negative() {
        let pos = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        assert_eq!(decimal_parts_to_i128(&pos), 100);

        let neg = DecimalParts {
            is_positive: false,
            scale: 2,
            precision: 10,
            int_parts: vec![200],
        };
        assert_eq!(decimal_parts_to_i128(&neg), -200);
    }

    #[test]
    fn decimal_parts_multi_word() {
        let parts = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 38,
            // 2^32 + 1 = 4294967297
            int_parts: vec![1, 1],
        };
        assert_eq!(decimal_parts_to_i128(&parts), (1_i128 << 32) + 1);
    }

    #[test]
    fn tds_date_conversion() {
        // 2000-01-01 is day 730119 since 0001-01-01
        // Arrow epoch (1970-01-01) is day 719162 since 0001-01-01
        // So days_since_epoch = 730119 - 719162 = 10957
        let date = SqlDate::create(730_119).unwrap();
        assert_eq!(tds_date_to_arrow_date32(&date), 10_957);
    }

    #[test]
    fn sql_time_to_micros_conversion() {
        // 1 second = 1_000_000_000 nanoseconds → 1_000_000 microseconds
        let time = SqlTime {
            time_nanoseconds: 1_000_000_000,
            scale: 7,
        };
        assert_eq!(sql_time_to_micros(&time), 1_000_000);
    }

    #[test]
    fn datetime_conversion() {
        // SqlDateTime: days from 1900-01-01, time in 1/300s ticks
        let dt = SqlDateTime {
            days: DAYS_1900_TO_UNIX, // at Unix epoch
            time: 300,               // 1 second
        };
        let micros = datetime_to_epoch_micros(&dt);
        assert_eq!(micros, 1_000_000);
    }

    #[test]
    fn smalldatetime_conversion() {
        let dt = SqlSmallDateTime {
            days: DAYS_1900_TO_UNIX as u16, // u16 required for SmallDateTime
            time: 1,                        // 1 minute after midnight
        };
        assert_eq!(smalldatetime_to_epoch_micros(&dt), MICROS_PER_MINUTE);
    }

    #[test]
    fn money_conversion() {
        // $1.00 = 10000 in MONEY wire format
        // TDS: msb_part = 0 (high), lsb_part = 10000 (low)
        let m = SqlMoney {
            lsb_part: 10_000,
            msb_part: 0,
        };
        assert!((sql_money_to_f64(&m) - 1.0).abs() < 1e-10);
    }
}

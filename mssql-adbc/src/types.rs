// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use adbc_core::error::{Error, Result};
use arrow_array::RecordBatch;
use arrow_array::builder::*;
use arrow_schema::{DataType, Field, Fields, Schema, SchemaRef, TimeUnit};
use mssql_tds::datatypes::column_values::*;
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::row_writer::RowWriter;
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_string::EncodingType;
use mssql_tds::datatypes::sql_vector::SqlVector;
use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::query::metadata::ColumnMetadata;
use uuid::Uuid;

use crate::error::{data, unsupported};

pub(crate) const EPOCH_DAYS: i64 = 719_162;
pub(crate) const TICKS_PER_DAY: i64 = 864_000_000_000;

pub(crate) fn datetime_fields(offset: bool) -> Fields {
    let mut fields = vec![
        Field::new("date", DataType::Date32, false),
        Field::new("time", DataType::Time64(TimeUnit::Nanosecond), false),
    ];
    if offset {
        fields.push(Field::new("offset_minutes", DataType::Int16, false));
    }
    fields.into()
}

pub(crate) fn field(column: &ColumnMetadata) -> Result<Field> {
    use TdsDataType::*;
    let length = column.type_info.length;
    let ty = match column.data_type {
        Bit | BitN => DataType::Boolean,
        Int1 => DataType::UInt8,
        Int2 => DataType::Int16,
        Int4 => DataType::Int32,
        Int8 => DataType::Int64,
        IntN => match length {
            1 => DataType::UInt8,
            2 => DataType::Int16,
            4 => DataType::Int32,
            8 => DataType::Int64,
            _ => return Err(data("invalid nullable integer length")),
        },
        Flt4 => DataType::Float32,
        Flt8 => DataType::Float64,
        FltN => match length {
            4 => DataType::Float32,
            8 => DataType::Float64,
            _ => return Err(data("invalid nullable float length")),
        },
        Decimal | DecimalN | Numeric | NumericN => DataType::Decimal128(
            column
                .get_precision()
                .ok_or_else(|| data("decimal precision missing"))?,
            column
                .get_scale()
                .ok_or_else(|| data("decimal scale missing"))? as i8,
        ),
        Money | Money4 | MoneyN => DataType::Decimal128(column.get_precision().unwrap_or(19), 4),
        Char | VarChar | BigChar | BigVarChar | NChar | NVarChar | Text | NText | Xml | Json => {
            DataType::Utf8
        }
        Binary | VarBinary | BigBinary | BigVarBinary | Image => DataType::Binary,
        Guid => DataType::FixedSizeBinary(16),
        DateN => DataType::Date32,
        TimeN => DataType::Time64(TimeUnit::Nanosecond),
        DateTime2N => DataType::Struct(datetime_fields(false)),
        DateTimeOffsetN => DataType::Struct(datetime_fields(true)),
        DateTime | DateTim4 | DateTimeN => DataType::Timestamp(TimeUnit::Microsecond, Option::None),
        _ => return Err(unsupported(format!("SQL type {:?}", column.data_type))),
    };
    let mut metadata =
        HashMap::from([("mssql.tds_type".into(), format!("{:?}", column.data_type))]);
    if let Some(scale) = column.get_scale() {
        metadata.insert("mssql.scale".into(), scale.to_string());
    }
    if let Some(precision) = column.get_precision() {
        metadata.insert("mssql.precision".into(), precision.to_string());
    }
    Ok(Field::new(&column.column_name, ty, column.is_nullable()).with_metadata(metadata))
}

pub(crate) fn schema(columns: &[ColumnMetadata]) -> Result<SchemaRef> {
    Ok(Arc::new(Schema::new(
        columns.iter().map(field).collect::<Result<Vec<_>>>()?,
    )))
}

pub(crate) struct ArrowRowWriter {
    schema: SchemaRef,
    builders: Vec<Box<dyn ArrayBuilder>>,
    pub(crate) rows: usize,
    pub(crate) bytes: usize,
    pub(crate) error: Option<Error>,
}

impl ArrowRowWriter {
    pub(crate) fn new(schema: SchemaRef, capacity: usize) -> Self {
        let builders = schema
            .fields()
            .iter()
            .map(|field| make_builder(field.data_type(), capacity))
            .collect();
        Self {
            schema,
            builders,
            rows: 0,
            bytes: 0,
            error: None,
        }
    }

    fn apply<B: ArrayBuilder>(&mut self, col: usize, work: impl FnOnce(&mut B)) {
        if self.error.is_some() {
            return;
        }
        match self
            .builders
            .get_mut(col)
            .and_then(|builder| builder.as_any_mut().downcast_mut::<B>())
        {
            Some(builder) => work(builder),
            None => self.error = Some(data(format!("decoded value does not match column {col}"))),
        }
    }

    fn fail(&mut self, error: Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn temporal(&mut self, col: usize, value: SqlDateTime2, offset: Option<i16>) {
        let day = i64::from(value.days) - EPOCH_DAYS;
        let Some(nanos) = value
            .time
            .time_nanoseconds
            .checked_mul(100)
            .and_then(|n| i64::try_from(n).ok())
        else {
            self.fail(data("time value overflows Arrow time64"));
            return;
        };
        if !(-EPOCH_DAYS..=3_652_058 - EPOCH_DAYS).contains(&day) || nanos >= 86_400_000_000_000 {
            self.fail(data("date/time outside SQL Server range"));
            return;
        }
        self.apply::<StructBuilder>(col, |builder| {
            builder
                .field_builder::<Date32Builder>(0)
                .expect("date builder")
                .append_value(day as i32);
            builder
                .field_builder::<Time64NanosecondBuilder>(1)
                .expect("time builder")
                .append_value(nanos);
            if let Some(offset) = offset {
                builder
                    .field_builder::<Int16Builder>(2)
                    .expect("offset builder")
                    .append_value(offset);
            }
            builder.append(true);
        });
    }

    pub(crate) fn finish(mut self) -> Result<RecordBatch> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let arrays = self
            .builders
            .iter_mut()
            .map(|builder| builder.finish())
            .collect();
        RecordBatch::try_new(self.schema, arrays).map_err(Into::into)
    }
}

macro_rules! primitive {
    ($name:ident, $builder:ty, $value:ty) => {
        fn $name(&mut self, col: usize, value: $value) {
            self.apply::<$builder>(col, |builder| builder.append_value(value));
        }
    };
}

impl RowWriter for ArrowRowWriter {
    primitive!(write_bool, BooleanBuilder, bool);
    primitive!(write_u8, UInt8Builder, u8);
    primitive!(write_i16, Int16Builder, i16);
    primitive!(write_i32, Int32Builder, i32);
    primitive!(write_i64, Int64Builder, i64);
    primitive!(write_f32, Float32Builder, f32);
    primitive!(write_f64, Float64Builder, f64);

    fn write_null(&mut self, col: usize) {
        if self.error.is_some() {
            return;
        }
        let Some(builder) = self.builders.get_mut(col) else {
            self.fail(data("invalid NULL column"));
            return;
        };
        let Some(field) = self.schema.fields().get(col) else {
            self.fail(data("missing NULL field"));
            return;
        };
        macro_rules! null {
            ($ty:ty) => {
                builder
                    .as_any_mut()
                    .downcast_mut::<$ty>()
                    .expect("schema builder")
                    .append_null()
            };
        }
        match field.data_type() {
            DataType::Boolean => null!(BooleanBuilder),
            DataType::UInt8 => null!(UInt8Builder),
            DataType::Int16 => null!(Int16Builder),
            DataType::Int32 => null!(Int32Builder),
            DataType::Int64 => null!(Int64Builder),
            DataType::Float32 => null!(Float32Builder),
            DataType::Float64 => null!(Float64Builder),
            DataType::Decimal128(_, _) => null!(Decimal128Builder),
            DataType::Utf8 => null!(StringBuilder),
            DataType::Binary => null!(BinaryBuilder),
            DataType::FixedSizeBinary(_) => null!(FixedSizeBinaryBuilder),
            DataType::Date32 => null!(Date32Builder),
            DataType::Time64(_) => null!(Time64NanosecondBuilder),
            DataType::Timestamp(_, _) => null!(TimestampMicrosecondBuilder),
            DataType::Struct(fields) => {
                let builder = builder
                    .as_any_mut()
                    .downcast_mut::<StructBuilder>()
                    .expect("struct builder");
                builder
                    .field_builder::<Date32Builder>(0)
                    .expect("date builder")
                    .append_null();
                builder
                    .field_builder::<Time64NanosecondBuilder>(1)
                    .expect("time builder")
                    .append_null();
                if fields.len() == 3 {
                    builder
                        .field_builder::<Int16Builder>(2)
                        .expect("offset builder")
                        .append_null();
                }
                builder.append(false);
            }
            _ => self.fail(unsupported("NULL conversion")),
        }
    }

    fn write_string(&mut self, col: usize, bytes: Cow<'_, [u8]>, encoding: EncodingType) {
        let Some(encoding) = encoding.resolved_encoding() else {
            self.fail(data("unresolved string encoding"));
            return;
        };
        let (text, malformed) = encoding.decode_without_bom_handling(&bytes);
        if malformed {
            self.fail(data("malformed SQL character data"));
            return;
        }
        if text.len() > i32::MAX as usize - self.bytes.min(i32::MAX as usize) {
            self.fail(data("Arrow string offset limit exceeded"));
            return;
        }
        self.bytes = self.bytes.saturating_add(text.len());
        self.apply::<StringBuilder>(col, |builder| builder.append_value(&text));
    }

    fn write_bytes(&mut self, col: usize, bytes: Cow<'_, [u8]>) {
        if bytes.len() > i32::MAX as usize - self.bytes.min(i32::MAX as usize) {
            self.fail(data("Arrow binary offset limit exceeded"));
            return;
        }
        self.bytes = self.bytes.saturating_add(bytes.len());
        self.apply::<BinaryBuilder>(col, |builder| builder.append_value(&bytes));
    }

    fn write_decimal(&mut self, col: usize, value: DecimalParts) {
        match i128::try_from(value.magnitude()) {
            Ok(magnitude) => self.apply::<Decimal128Builder>(col, |builder| {
                builder.append_value(if value.is_positive {
                    magnitude
                } else {
                    -magnitude
                });
            }),
            Err(_) => self.fail(data("decimal magnitude exceeds Decimal128")),
        }
    }
    fn write_numeric(&mut self, col: usize, value: DecimalParts) {
        self.write_decimal(col, value);
    }
    fn write_money(&mut self, col: usize, value: SqlMoney) {
        let scaled = (i64::from(value.msb_part) << 32) | i64::from(value.lsb_part as u32);
        self.apply::<Decimal128Builder>(col, |builder| builder.append_value(i128::from(scaled)));
    }
    fn write_smallmoney(&mut self, col: usize, value: SqlSmallMoney) {
        self.apply::<Decimal128Builder>(col, |builder| {
            builder.append_value(i128::from(value.int_val))
        });
    }
    fn write_date(&mut self, col: usize, value: SqlDate) {
        self.apply::<Date32Builder>(col, |builder| {
            builder.append_value((i64::from(value.get_days()) - EPOCH_DAYS) as i32)
        });
    }
    fn write_time(&mut self, col: usize, value: SqlTime) {
        match value
            .time_nanoseconds
            .checked_mul(100)
            .and_then(|n| i64::try_from(n).ok())
            .filter(|n| *n < 86_400_000_000_000)
        {
            Some(nanos) => {
                self.apply::<Time64NanosecondBuilder>(col, |builder| builder.append_value(nanos))
            }
            None => self.fail(data("invalid SQL time")),
        }
    }
    fn write_datetime2(&mut self, col: usize, value: SqlDateTime2) {
        self.temporal(col, value, None);
    }
    fn write_datetimeoffset(&mut self, col: usize, value: SqlDateTimeOffset) {
        self.temporal(col, value.datetime2, Some(value.offset));
    }
    fn write_datetime(&mut self, col: usize, value: SqlDateTime) {
        let micros = (i64::from(value.days) - 25_567) * 86_400_000_000
            + (i64::from(value.time) * 1_000_000 + 150) / 300;
        self.apply::<TimestampMicrosecondBuilder>(col, |builder| builder.append_value(micros));
    }
    fn write_smalldatetime(&mut self, col: usize, value: SqlSmallDateTime) {
        let micros =
            (i64::from(value.days) - 25_567) * 86_400_000_000 + i64::from(value.time) * 60_000_000;
        self.apply::<TimestampMicrosecondBuilder>(col, |builder| builder.append_value(micros));
    }
    fn write_uuid(&mut self, col: usize, value: Uuid) {
        self.apply::<FixedSizeBinaryBuilder>(col, |builder| {
            builder
                .append_value(value.as_bytes())
                .expect("16-byte UUID")
        });
    }
    fn write_xml(&mut self, col: usize, value: SqlXml) {
        self.write_string(col, Cow::Borrowed(&value.bytes), EncodingType::Utf16);
    }
    fn write_json(&mut self, col: usize, value: SqlJson) {
        self.write_string(col, Cow::Borrowed(&value.bytes), EncodingType::Utf8);
    }
    fn write_vector(&mut self, _col: usize, _value: SqlVector) {
        self.fail(unsupported("vectors"));
    }
    fn end_row(&mut self) {
        self.rows += 1;
        self.bytes = self
            .bytes
            .saturating_add(self.builders.len().saturating_mul(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, Decimal128Array, StructArray, Time64NanosecondArray};

    #[test]
    fn time_uses_100ns_ticks_and_datetime2_preserves_full_range() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "t",
            DataType::Struct(datetime_fields(false)),
            true,
        )]));
        let mut writer = ArrowRowWriter::new(schema, 2);
        writer.write_datetime2(
            0,
            SqlDateTime2 {
                days: 3_652_058,
                time: SqlTime {
                    time_nanoseconds: 123,
                    scale: 7,
                },
            },
        );
        writer.end_row();
        writer.write_null(0);
        writer.end_row();
        let batch = writer.finish().unwrap();
        let values = batch
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        assert!(values.is_null(1));
        let times = values
            .column(1)
            .as_any()
            .downcast_ref::<Time64NanosecondArray>()
            .unwrap();
        assert_eq!(times.value(0), 12_300);
    }

    #[test]
    fn money_does_not_round_through_float() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "m",
            DataType::Decimal128(19, 4),
            false,
        )]));
        let mut writer = ArrowRowWriter::new(schema, 1);
        writer.write_money(
            0,
            SqlMoney {
                msb_part: i32::MAX,
                lsb_part: -1,
            },
        );
        writer.end_row();
        let batch = writer.finish().unwrap();
        assert_eq!(
            batch
                .column(0)
                .as_any()
                .downcast_ref::<Decimal128Array>()
                .unwrap()
                .value(0),
            i128::from(i64::MAX)
        );
    }

    #[test]
    fn malformed_text_is_an_error_not_a_panic_or_replacement() {
        for bytes in [Cow::Borrowed(&[0xff][..]), Cow::Owned(vec![0xff])] {
            let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
            let mut writer = ArrowRowWriter::new(schema, 1);
            writer.write_string(0, bytes, EncodingType::Utf8);
            writer.end_row();
            assert!(writer.finish().is_err());
        }
    }
}

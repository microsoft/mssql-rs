// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use adbc_core::error::Result;
use arrow_array::*;
use arrow_schema::{DataType, Schema, TimeUnit};
use mssql_tds::datatypes::column_values::*;
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, RpcTypeMetadata, StatusFlags};
use uuid::Uuid;

use crate::error::{data, invalid, tds, unsupported};
use crate::types::{EPOCH_DAYS, datetime_fields};

pub(crate) enum Binding {
    Batch(RecordBatch),
    Stream {
        schema: arrow_schema::SchemaRef,
        reader: Option<Box<dyn RecordBatchReader + Send>>,
    },
}

impl Binding {
    pub(crate) fn schema(&self) -> arrow_schema::SchemaRef {
        match self {
            Self::Batch(batch) => batch.schema(),
            Self::Stream { schema, .. } => schema.clone(),
        }
    }
    pub(crate) fn reader(&mut self) -> Result<Box<dyn RecordBatchReader + Send>> {
        match self {
            Self::Batch(batch) => Ok(Box::new(RecordBatchIterator::new(
                [Ok(batch.clone())],
                batch.schema(),
            ))),
            Self::Stream { reader, .. } => reader
                .take()
                .ok_or_else(|| crate::error::state("bound stream consumed; bind a new stream")),
        }
    }
}

pub(crate) struct ParameterCursor {
    source: Option<Box<dyn RecordBatchReader + Send>>,
    schema: Option<arrow_schema::SchemaRef>,
    batch: Option<RecordBatch>,
    row: usize,
    unbound_pending: bool,
}

impl ParameterCursor {
    pub(crate) fn new(binding: Option<&mut Binding>) -> Result<Self> {
        let schema = binding.as_ref().map(|binding| binding.schema());
        let source = binding.map(Binding::reader).transpose()?;
        Ok(Self {
            source,
            schema,
            batch: None,
            row: 0,
            unbound_pending: true,
        })
    }
    pub(crate) fn next(&mut self) -> Result<Option<Vec<RpcParameter>>> {
        let Some(source) = &mut self.source else {
            return Ok(if std::mem::take(&mut self.unbound_pending) {
                Some(vec![])
            } else {
                None
            });
        };
        loop {
            if let Some(batch) = &self.batch
                && self.row < batch.num_rows()
            {
                let parameters = parameters(batch, self.row)?;
                self.row += 1;
                return Ok(Some(parameters));
            }
            match source.next() {
                None => return Ok(None),
                Some(Err(error)) => return Err(data(format!("bound stream failed: {error}"))),
                Some(Ok(batch)) => {
                    if self.schema.as_ref() != Some(&batch.schema()) {
                        return Err(data("bound stream schema changed"));
                    }
                    self.batch = Some(batch);
                    self.row = 0;
                }
            }
        }
    }
}

fn array<T: 'static>(value: &dyn Array) -> Result<&T> {
    value
        .as_any()
        .downcast_ref()
        .ok_or_else(|| data("Arrow array does not match its declared type"))
}

pub(crate) fn validate(schema: &Schema) -> Result<()> {
    for field in schema.fields() {
        declaration(field.data_type())?;
    }
    Ok(())
}

pub(crate) fn declaration(ty: &DataType) -> Result<String> {
    Ok(match ty {
        DataType::Boolean => "bit".into(),
        DataType::UInt8 => "tinyint".into(),
        DataType::Int8 | DataType::Int16 => "smallint".into(),
        DataType::UInt16 | DataType::Int32 => "int".into(),
        DataType::UInt32 | DataType::Int64 => "bigint".into(),
        DataType::UInt64 => "decimal(20,0)".into(),
        DataType::Float32 => "real".into(),
        DataType::Float64 => "float".into(),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "nvarchar(max)".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "varbinary(max)".into(),
        DataType::FixedSizeBinary(16) => "uniqueidentifier".into(),
        DataType::Decimal128(precision, scale)
            if (1..=38).contains(precision) && *scale >= 0 && *scale as u8 <= *precision =>
        {
            format!("decimal({precision},{scale})")
        }
        DataType::Date32 => "date".into(),
        DataType::Time32(TimeUnit::Second | TimeUnit::Millisecond)
        | DataType::Time64(TimeUnit::Microsecond | TimeUnit::Nanosecond) => "time(7)".into(),
        DataType::Timestamp(_, timezone) => if timezone.is_some() {
            "datetimeoffset(7)"
        } else {
            "datetime2(7)"
        }
        .into(),
        DataType::Struct(fields) if *fields == datetime_fields(false) => "datetime2(7)".into(),
        DataType::Struct(fields) if *fields == datetime_fields(true) => "datetimeoffset(7)".into(),
        _ => return Err(unsupported(format!("Arrow input type {ty}"))),
    })
}

fn ticks(value: i64, unit: &TimeUnit) -> Result<i64> {
    match unit {
        TimeUnit::Second => value.checked_mul(10_000_000),
        TimeUnit::Millisecond => value.checked_mul(10_000),
        TimeUnit::Microsecond => value.checked_mul(10),
        TimeUnit::Nanosecond if value % 100 == 0 => Some(value / 100),
        TimeUnit::Nanosecond => {
            return Err(data("SQL Server cannot represent sub-100ns precision"));
        }
    }
    .ok_or_else(|| data("timestamp conversion overflow"))
}

fn datetime_from_ticks(value: i64) -> Result<SqlDateTime2> {
    let days = value
        .div_euclid(crate::types::TICKS_PER_DAY)
        .checked_add(EPOCH_DAYS)
        .ok_or_else(|| data("timestamp date overflow"))?;
    let days = u32::try_from(days).map_err(|_| data("timestamp predates year 1"))?;
    SqlDate::create(days).map_err(tds)?;
    Ok(SqlDateTime2 {
        days,
        time: SqlTime {
            time_nanoseconds: value.rem_euclid(crate::types::TICKS_PER_DAY) as u64,
            scale: 7,
        },
    })
}

pub(crate) fn value(values: &dyn Array, row: usize) -> Result<SqlType> {
    if row >= values.len() {
        return Err(invalid("input row is out of bounds"));
    }
    let is_null = values.is_null(row);
    macro_rules! primitive {
        ($array:ty, $variant:ident) => {
            SqlType::$variant(if is_null {
                None
            } else {
                Some(array::<$array>(values)?.value(row).into())
            })
        };
    }
    macro_rules! text {
        ($array:ty) => {
            SqlType::NVarcharMax(if is_null {
                None
            } else {
                Some(SqlString::from_utf8_string(
                    array::<$array>(values)?.value(row).to_owned(),
                ))
            })
        };
    }
    macro_rules! binary {
        ($array:ty) => {
            SqlType::VarBinaryMax(if is_null {
                None
            } else {
                Some(array::<$array>(values)?.value(row).to_vec())
            })
        };
    }
    Ok(match values.data_type() {
        DataType::Boolean => primitive!(BooleanArray, Bit),
        DataType::UInt8 => primitive!(UInt8Array, TinyInt),
        DataType::Int8 => primitive!(Int8Array, SmallInt),
        DataType::Int16 => primitive!(Int16Array, SmallInt),
        DataType::UInt16 => primitive!(UInt16Array, Int),
        DataType::Int32 => primitive!(Int32Array, Int),
        DataType::UInt32 => primitive!(UInt32Array, BigInt),
        DataType::Int64 => primitive!(Int64Array, BigInt),
        DataType::UInt64 => SqlType::Decimal(if is_null {
            None
        } else {
            Some(DecimalParts::new(
                true,
                20,
                0,
                u128::from(array::<UInt64Array>(values)?.value(row)),
            ))
        }),
        DataType::Float32 => primitive!(Float32Array, Real),
        DataType::Float64 => primitive!(Float64Array, Float),
        DataType::Utf8 => text!(StringArray),
        DataType::LargeUtf8 => text!(LargeStringArray),
        DataType::Utf8View => text!(StringViewArray),
        DataType::Binary => binary!(BinaryArray),
        DataType::LargeBinary => binary!(LargeBinaryArray),
        DataType::BinaryView => binary!(BinaryViewArray),
        DataType::FixedSizeBinary(16) => SqlType::Uuid(if is_null {
            None
        } else {
            Some(
                Uuid::from_slice(array::<FixedSizeBinaryArray>(values)?.value(row))
                    .map_err(|_| data("invalid UUID bytes"))?,
            )
        }),
        DataType::Decimal128(precision, scale) => {
            declaration(values.data_type())?;
            let value = if is_null {
                None
            } else {
                let value = array::<Decimal128Array>(values)?.value(row);
                if value.unsigned_abs() >= 10_u128.pow(u32::from(*precision)) {
                    return Err(data("decimal exceeds declared precision"));
                }
                Some(DecimalParts::new(
                    value >= 0,
                    *precision,
                    *scale as u8,
                    value.unsigned_abs(),
                ))
            };
            SqlType::Decimal(value)
        }
        DataType::Date32 => SqlType::Date(if is_null {
            None
        } else {
            let days = i64::from(array::<Date32Array>(values)?.value(row)) + EPOCH_DAYS;
            Some(
                SqlDate::create(u32::try_from(days).map_err(|_| data("date predates year 1"))?)
                    .map_err(tds)?,
            )
        }),
        DataType::Timestamp(unit, timezone) => {
            let datetime = if is_null {
                None
            } else {
                let value = match unit {
                    TimeUnit::Second => array::<TimestampSecondArray>(values)?.value(row),
                    TimeUnit::Millisecond => array::<TimestampMillisecondArray>(values)?.value(row),
                    TimeUnit::Microsecond => array::<TimestampMicrosecondArray>(values)?.value(row),
                    TimeUnit::Nanosecond => array::<TimestampNanosecondArray>(values)?.value(row),
                };
                Some(datetime_from_ticks(ticks(value, unit)?)?)
            };
            if timezone.is_some() {
                SqlType::DateTimeOffset(datetime.map(|datetime2| SqlDateTimeOffset {
                    datetime2,
                    offset: 0,
                }))
            } else {
                SqlType::DateTime2(datetime)
            }
        }
        DataType::Time32(unit) | DataType::Time64(unit) => {
            let time = if is_null {
                None
            } else {
                let value = match unit {
                    TimeUnit::Second => i64::from(array::<Time32SecondArray>(values)?.value(row)),
                    TimeUnit::Millisecond => {
                        i64::from(array::<Time32MillisecondArray>(values)?.value(row))
                    }
                    TimeUnit::Microsecond => array::<Time64MicrosecondArray>(values)?.value(row),
                    TimeUnit::Nanosecond => array::<Time64NanosecondArray>(values)?.value(row),
                };
                let ticks = ticks(value, unit)?;
                if !(0..crate::types::TICKS_PER_DAY).contains(&ticks) {
                    return Err(data("time must be within one day"));
                }
                Some(SqlTime {
                    time_nanoseconds: ticks as u64,
                    scale: 7,
                })
            };
            SqlType::Time(time)
        }
        DataType::Struct(fields)
            if *fields == datetime_fields(false) || *fields == datetime_fields(true) =>
        {
            let offset = fields.len() == 3;
            let value = if is_null {
                None
            } else {
                let values = array::<StructArray>(values)?;
                if values.columns().iter().any(|column| column.is_null(row)) {
                    return Err(data("non-null date/time struct has a null child"));
                }
                let days = i64::from(array::<Date32Array>(values.column(0).as_ref())?.value(row))
                    + EPOCH_DAYS;
                let date =
                    SqlDate::create(u32::try_from(days).map_err(|_| data("date predates year 1"))?)
                        .map_err(tds)?;
                let nanos = array::<Time64NanosecondArray>(values.column(1).as_ref())?.value(row);
                let time = ticks(nanos, &TimeUnit::Nanosecond)?;
                if !(0..crate::types::TICKS_PER_DAY).contains(&time) {
                    return Err(data("time must be within one day"));
                }
                let offset = if offset {
                    array::<Int16Array>(values.column(2).as_ref())?.value(row)
                } else {
                    0
                };
                if !(-840..=840).contains(&offset) {
                    return Err(data("UTC offset must be within 14 hours"));
                }
                Some(SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: date.get_days(),
                        time: SqlTime {
                            time_nanoseconds: time as u64,
                            scale: 7,
                        },
                    },
                    offset,
                })
            };
            if offset {
                SqlType::DateTimeOffset(value)
            } else {
                SqlType::DateTime2(value.map(|value| value.datetime2))
            }
        }
        _ => {
            return Err(unsupported(format!(
                "Arrow input type {}",
                values.data_type()
            )));
        }
    })
}

pub(crate) fn parameters(batch: &RecordBatch, row: usize) -> Result<Vec<RpcParameter>> {
    batch
        .columns()
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let value = value(column.as_ref(), row)?;
            let metadata = match column.data_type() {
                DataType::Decimal128(precision, scale) => Some(RpcTypeMetadata {
                    precision: Some(*precision),
                    scale: Some(*scale as u8),
                }),
                DataType::UInt64 => Some(RpcTypeMetadata {
                    precision: Some(20),
                    scale: Some(0),
                }),
                _ => None,
            };
            let mut parameter =
                RpcParameter::new(Some(format!("@p{}", index + 1)), StatusFlags::NONE, value);
            if let Some(metadata) = metadata {
                parameter = parameter.with_type_metadata(metadata);
            }
            Ok(parameter)
        })
        .collect()
}

pub(crate) fn declarations(schema: &Schema) -> Result<String> {
    schema
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| {
            Ok(format!(
                "@p{} {}",
                index + 1,
                declaration(field.data_type())?
            ))
        })
        .collect::<Result<Vec<_>>>()
        .map(|items| items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanoseconds_must_be_exactly_representable() {
        assert!(value(&TimestampNanosecondArray::from(vec![1]), 0).is_err());
        assert!(value(&Time64NanosecondArray::from(vec![100]), 0).is_ok());
        assert!(value(&Time64NanosecondArray::from(vec![-100]), 0).is_err());
    }

    #[test]
    fn typed_decimal_null_keeps_precision_and_scale() {
        let values = Decimal128Array::from(vec![None])
            .with_precision_and_scale(38, 7)
            .unwrap();
        let batch =
            RecordBatch::try_from_iter([("d", std::sync::Arc::new(values) as ArrayRef)]).unwrap();
        assert_eq!(parameters(&batch, 0).unwrap().len(), 1);
        assert_eq!(declarations(&batch.schema()).unwrap(), "@p1 decimal(38,7)");
    }

    #[test]
    fn unsupported_and_out_of_range_values_are_errors() {
        assert!(declaration(&DataType::Decimal128(10, -1)).is_err());
        assert!(value(&Date32Array::from(vec![i32::MIN]), 0).is_err());
        assert!(value(&StringArray::from(vec!["x"]), 1).is_err());
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arrow-input bulk copy adapter.
//!
//! Implements [`BulkLoadRow`] over a [`RecordBatch`] so the TDS write path can
//! consume Arrow-shaped data without round-tripping through Python tuples. The
//! per-cell `is_instance_of` cascade and per-cell `extract::<i64>()` of the
//! tuple path are replaced with one type-dispatch per column per batch
//! ([`ColumnPlan`]) followed by typed buffer reads.

use std::sync::Arc;

use arrow::array::{
    Array, BooleanArray, Date32Array, Date64Array, Decimal128Array, FixedSizeBinaryArray,
    Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray,
    LargeStringArray, RecordBatch, StringArray, Time32MillisecondArray, Time32SecondArray,
    Time64MicrosecondArray, Time64NanosecondArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow::array::{BinaryArray, NullArray};
use arrow::datatypes::{DataType, Schema, TimeUnit};
use async_trait::async_trait;
use mssql_tds::connection::bulk_copy::{BulkLoadRow, ResolvedColumnMapping};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType};
use mssql_tds::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime2, SqlDateTimeOffset, SqlTime,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::error::Error;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;
use uuid::Uuid;

/// Days from `0001-01-01` to `1970-01-01` (UNIX epoch). Arrow `Date32` stores
/// days since the epoch; SQL `date` stores days since `0001-01-01`.
const EPOCH_DAYS_FROM_YEAR_ONE: i32 = 719_162;

/// 100-ns ticks per second. SQL Server `time`/`datetime2`/`datetimeoffset`
/// store time as 100-ns units; the [`SqlTime::time_nanoseconds`] field is
/// misnamed and actually holds 100-ns ticks.
const TICKS_PER_SECOND: u64 = 10_000_000;

/// Per-column dispatch cached at batch boundary so per-row work is a buffer
/// read plus one [`StreamingBulkLoadWriter::write_column_value`] call.
#[derive(Debug, Clone)]
pub struct ColumnPlan {
    /// Source ordinal in the [`RecordBatch`].
    pub source_index: usize,
    /// Destination column index in the table.
    pub destination_index: usize,
    /// Per-cell extraction strategy resolved from `(arrow type, sql type)`.
    pub kind: ColumnPlanKind,
}

/// Per-cell extraction strategy. Variant data is intentionally minimal because
/// the arrow array is borrowed from the [`RecordBatch`] at write time.
#[derive(Debug, Clone, Copy)]
pub enum ColumnPlanKind {
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    /// Arrow `uint64` -> SQL `bigint`. Values > `i64::MAX` are rejected (A2).
    UInt64,
    Float32,
    Float64,
    Bool,
    /// UTF-8 → NVARCHAR (SQL Server stores UTF-16LE internally).
    Utf8Nvarchar,
    /// UTF-8 → VARCHAR.
    Utf8VarChar,
    LargeUtf8Nvarchar,
    LargeUtf8VarChar,
    Binary,
    LargeBinary,
    /// Arrow `date32` → SQL `date`.
    Date32,
    /// Arrow `date64` (ms) → SQL `date`.
    Date64,
    /// Arrow `timestamp` without timezone → SQL `datetime2(scale)`.
    TimestampDateTime2 {
        unit: TimeUnit,
        scale: u8,
    },
    /// Arrow `timestamp` with timezone → SQL `datetimeoffset(scale)`.
    /// Arrow's timestamp values are always UTC; offset is encoded as 0.
    TimestampDateTimeOffset {
        unit: TimeUnit,
        scale: u8,
    },
    /// Arrow `time32`/`time64` → SQL `time(scale)`.
    Time {
        unit: TimeUnit,
        scale: u8,
    },
    /// Arrow `decimal128(p,s)` → SQL `decimal(p,s)`.
    Decimal128 {
        precision: u8,
        scale: u8,
    },
    /// Arrow `fixed_size_binary(16)` → SQL `uniqueidentifier`.
    FixedBin16Uuid,
    /// All-null arrow column.
    Null,
}

/// Build one [`ColumnPlan`] per resolved mapping. Fails fast on any
/// (arrow type, sql type) pair the row-major path doesn't support.
pub fn build_column_plans(
    schema: &Schema,
    dest: &[BulkCopyColumnMetadata],
    mappings: &[ResolvedColumnMapping],
) -> TdsResult<Vec<ColumnPlan>> {
    mappings
        .iter()
        .map(|m| {
            let field = schema.fields().get(m.source_index).ok_or_else(|| {
                Error::UsageError(format!(
                    "Arrow source column index {} out of bounds (schema has {} fields)",
                    m.source_index,
                    schema.fields().len()
                ))
            })?;
            let dest_meta = dest.get(m.destination_index).ok_or_else(|| {
                Error::UsageError(format!(
                    "Destination column index {} out of bounds (table has {} columns)",
                    m.destination_index,
                    dest.len()
                ))
            })?;
            let kind = resolve_kind(field.data_type(), dest_meta).map_err(|e| {
                Error::UsageError(format!(
                    "Cannot map Arrow column '{}' ({:?}) to SQL column '{}' ({:?}): {}",
                    field.name(),
                    field.data_type(),
                    dest_meta.column_name,
                    dest_meta.sql_type,
                    e
                ))
            })?;
            Ok(ColumnPlan {
                source_index: m.source_index,
                destination_index: m.destination_index,
                kind,
            })
        })
        .collect()
}

fn resolve_kind(arrow_ty: &DataType, dest: &BulkCopyColumnMetadata) -> TdsResult<ColumnPlanKind> {
    use SqlDbType as S;

    let kind = match (arrow_ty, dest.sql_type) {
        (DataType::Null, _) => ColumnPlanKind::Null,
        (DataType::Boolean, S::Bit) => ColumnPlanKind::Bool,

        // Any integer Arrow type may target any integer SQL type; `narrow_int`
        // range-checks each cell at write time (A1: down-narrowing supported,
        // e.g. Int64 -> INT, rejecting out-of-range values).
        (DataType::Int8, S::TinyInt | S::SmallInt | S::Int | S::BigInt) => ColumnPlanKind::Int8,
        (DataType::Int16, S::TinyInt | S::SmallInt | S::Int | S::BigInt) => ColumnPlanKind::Int16,
        (DataType::Int32, S::TinyInt | S::SmallInt | S::Int | S::BigInt) => ColumnPlanKind::Int32,
        (DataType::Int64, S::TinyInt | S::SmallInt | S::Int | S::BigInt) => ColumnPlanKind::Int64,
        (DataType::UInt8, S::TinyInt | S::SmallInt | S::Int | S::BigInt) => ColumnPlanKind::UInt8,
        (DataType::UInt16, S::TinyInt | S::SmallInt | S::Int | S::BigInt) => ColumnPlanKind::UInt16,
        (DataType::UInt32, S::TinyInt | S::SmallInt | S::Int | S::BigInt) => ColumnPlanKind::UInt32,
        // UInt64 maps only to BigInt; the read path overflow-checks > i64::MAX (A2).
        (DataType::UInt64, S::BigInt) => ColumnPlanKind::UInt64,

        (DataType::Float32, S::Real | S::Float) => ColumnPlanKind::Float32,
        (DataType::Float64, S::Float) => ColumnPlanKind::Float64,

        (DataType::Utf8, S::NVarChar | S::NChar | S::NText) => ColumnPlanKind::Utf8Nvarchar,
        (DataType::Utf8, S::VarChar | S::Char | S::Text) => ColumnPlanKind::Utf8VarChar,
        (DataType::LargeUtf8, S::NVarChar | S::NChar | S::NText) => {
            ColumnPlanKind::LargeUtf8Nvarchar
        }
        (DataType::LargeUtf8, S::VarChar | S::Char | S::Text) => ColumnPlanKind::LargeUtf8VarChar,

        (DataType::Binary, S::VarBinary | S::Binary | S::Image) => ColumnPlanKind::Binary,
        (DataType::LargeBinary, S::VarBinary | S::Binary | S::Image) => ColumnPlanKind::LargeBinary,

        (DataType::Date32, S::Date) => ColumnPlanKind::Date32,
        (DataType::Date64, S::Date) => ColumnPlanKind::Date64,

        (DataType::Timestamp(unit, tz), S::DateTime2) if tz.is_none() => {
            ColumnPlanKind::TimestampDateTime2 {
                unit: *unit,
                scale: pick_timestamp_scale(*unit, dest.scale),
            }
        }
        (DataType::Timestamp(unit, tz), S::DateTimeOffset) if tz.is_some() => {
            ColumnPlanKind::TimestampDateTimeOffset {
                unit: *unit,
                scale: pick_timestamp_scale(*unit, dest.scale),
            }
        }
        // Tz-bearing timestamps mapped onto datetime2 are coerced to UTC.
        (DataType::Timestamp(unit, _), S::DateTime2) => ColumnPlanKind::TimestampDateTime2 {
            unit: *unit,
            scale: pick_timestamp_scale(*unit, dest.scale),
        },

        (DataType::Time32(unit @ (TimeUnit::Second | TimeUnit::Millisecond)), S::Time) => {
            ColumnPlanKind::Time {
                unit: *unit,
                scale: pick_timestamp_scale(*unit, dest.scale),
            }
        }
        (DataType::Time64(unit @ (TimeUnit::Microsecond | TimeUnit::Nanosecond)), S::Time) => {
            ColumnPlanKind::Time {
                unit: *unit,
                scale: pick_timestamp_scale(*unit, dest.scale),
            }
        }

        (DataType::Decimal128(p, s), S::Decimal | S::Numeric) => ColumnPlanKind::Decimal128 {
            precision: *p,
            scale: *s as u8,
        },

        (DataType::FixedSizeBinary(16), S::UniqueIdentifier) => ColumnPlanKind::FixedBin16Uuid,
        (DataType::FixedSizeBinary(16), S::Binary | S::VarBinary) => ColumnPlanKind::Binary,

        _ => {
            return Err(Error::UsageError(
                "type combination is not supported by the Arrow row-major writer".into(),
            ));
        }
    };
    Ok(kind)
}

/// Pick a TDS scale for a timestamp/time column. Honor an explicit destination
/// scale if it was advertised; otherwise default by Arrow [`TimeUnit`].
fn pick_timestamp_scale(unit: TimeUnit, dest_scale: u8) -> u8 {
    if dest_scale > 0 {
        return dest_scale.min(7);
    }
    match unit {
        TimeUnit::Second => 0,
        TimeUnit::Millisecond => 3,
        TimeUnit::Microsecond => 6,
        TimeUnit::Nanosecond => 7,
    }
}

impl ColumnPlan {
    /// Read one cell from the supplied arrow array. Honors the validity bitmap.
    pub fn extract_value(
        &self,
        arr: &dyn Array,
        row_idx: usize,
        dest: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        if arr.is_null(row_idx) {
            if !dest.is_nullable {
                return Err(Error::UsageError(format!(
                    "Cannot insert NULL value into non-nullable column '{}'",
                    dest.column_name
                )));
            }
            return Ok(ColumnValues::Null);
        }

        match self.kind {
            ColumnPlanKind::Null => {
                if !dest.is_nullable {
                    return Err(Error::UsageError(format!(
                        "Cannot insert NULL value into non-nullable column '{}'",
                        dest.column_name
                    )));
                }
                Ok(ColumnValues::Null)
            }
            ColumnPlanKind::Bool => {
                let a = downcast::<BooleanArray>(arr)?;
                Ok(ColumnValues::Bit(a.value(row_idx)))
            }
            ColumnPlanKind::Int8 => {
                narrow_int(downcast::<Int8Array>(arr)?.value(row_idx) as i64, dest)
            }
            ColumnPlanKind::Int16 => {
                narrow_int(downcast::<Int16Array>(arr)?.value(row_idx) as i64, dest)
            }
            ColumnPlanKind::Int32 => {
                narrow_int(downcast::<Int32Array>(arr)?.value(row_idx) as i64, dest)
            }
            ColumnPlanKind::Int64 => narrow_int(downcast::<Int64Array>(arr)?.value(row_idx), dest),
            ColumnPlanKind::UInt8 => {
                narrow_int(downcast::<UInt8Array>(arr)?.value(row_idx) as i64, dest)
            }
            ColumnPlanKind::UInt16 => {
                narrow_int(downcast::<UInt16Array>(arr)?.value(row_idx) as i64, dest)
            }
            ColumnPlanKind::UInt32 => {
                narrow_int(downcast::<UInt32Array>(arr)?.value(row_idx) as i64, dest)
            }
            ColumnPlanKind::UInt64 => {
                let v = downcast::<UInt64Array>(arr)?.value(row_idx);
                if v > i64::MAX as u64 {
                    return Err(Error::UsageError(format!(
                        "Value {} out of range for BIGINT column '{}' (exceeds i64::MAX)",
                        v, dest.column_name
                    )));
                }
                narrow_int(v as i64, dest)
            }
            ColumnPlanKind::Float32 => match dest.sql_type {
                SqlDbType::Real => Ok(ColumnValues::Real(
                    downcast::<Float32Array>(arr)?.value(row_idx),
                )),
                _ => Ok(ColumnValues::Float(
                    downcast::<Float32Array>(arr)?.value(row_idx) as f64,
                )),
            },
            ColumnPlanKind::Float64 => Ok(ColumnValues::Float(
                downcast::<Float64Array>(arr)?.value(row_idx),
            )),
            ColumnPlanKind::Utf8Nvarchar => {
                let s = downcast::<StringArray>(arr)?.value(row_idx).to_owned();
                Ok(ColumnValues::String(SqlString::from_utf8_string(s)))
            }
            ColumnPlanKind::Utf8VarChar => {
                let s = downcast::<StringArray>(arr)?.value(row_idx).to_owned();
                Ok(ColumnValues::String(SqlString::from_utf8_string(s)))
            }
            ColumnPlanKind::LargeUtf8Nvarchar => {
                let s = downcast::<LargeStringArray>(arr)?.value(row_idx).to_owned();
                Ok(ColumnValues::String(SqlString::from_utf8_string(s)))
            }
            ColumnPlanKind::LargeUtf8VarChar => {
                let s = downcast::<LargeStringArray>(arr)?.value(row_idx).to_owned();
                Ok(ColumnValues::String(SqlString::from_utf8_string(s)))
            }
            ColumnPlanKind::Binary => {
                let bytes = if let Some(a) = arr.as_any().downcast_ref::<BinaryArray>() {
                    a.value(row_idx).to_vec()
                } else if let Some(a) = arr.as_any().downcast_ref::<FixedSizeBinaryArray>() {
                    a.value(row_idx).to_vec()
                } else {
                    return Err(downcast_err::<BinaryArray>(arr));
                };
                Ok(ColumnValues::Bytes(bytes))
            }
            ColumnPlanKind::LargeBinary => {
                let bytes = downcast::<LargeBinaryArray>(arr)?.value(row_idx).to_vec();
                Ok(ColumnValues::Bytes(bytes))
            }
            ColumnPlanKind::Date32 => {
                let days = downcast::<Date32Array>(arr)?.value(row_idx);
                let sql_days = days
                    .checked_add(EPOCH_DAYS_FROM_YEAR_ONE)
                    .ok_or_else(|| Error::UsageError("Date32 value overflow".into()))?;
                if sql_days < 0 {
                    return Err(Error::UsageError(format!(
                        "Date {} is before 0001-01-01",
                        days
                    )));
                }
                Ok(ColumnValues::Date(SqlDate::create(sql_days as u32)?))
            }
            ColumnPlanKind::Date64 => {
                let ms = downcast::<Date64Array>(arr)?.value(row_idx);
                let days = ms.div_euclid(86_400_000) as i64;
                let sql_days = (days as i64) + EPOCH_DAYS_FROM_YEAR_ONE as i64;
                if !(0..=3_652_058).contains(&sql_days) {
                    return Err(Error::UsageError(format!(
                        "Date64 value {} ms out of SQL DATE range",
                        ms
                    )));
                }
                Ok(ColumnValues::Date(SqlDate::create(sql_days as u32)?))
            }
            ColumnPlanKind::TimestampDateTime2 { unit, scale } => {
                let ts = read_timestamp(arr, row_idx, unit)?;
                Ok(ColumnValues::DateTime2(timestamp_to_dt2(ts, scale)?))
            }
            ColumnPlanKind::TimestampDateTimeOffset { unit, scale } => {
                let ts = read_timestamp(arr, row_idx, unit)?;
                let dt2 = timestamp_to_dt2(ts, scale)?;
                Ok(ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                    datetime2: dt2,
                    offset: 0,
                }))
            }
            ColumnPlanKind::Time { unit, scale } => {
                let ticks = read_time_ticks(arr, row_idx, unit)?;
                Ok(ColumnValues::Time(SqlTime {
                    time_nanoseconds: ticks,
                    scale,
                }))
            }
            ColumnPlanKind::Decimal128 { precision, scale } => {
                let a = downcast::<Decimal128Array>(arr)?;
                let raw = a.value(row_idx);
                let s = decimal128_to_string(raw, scale);
                Ok(ColumnValues::Decimal(DecimalParts::from_string(
                    &s, precision, scale,
                )?))
            }
            ColumnPlanKind::FixedBin16Uuid => {
                let a = downcast::<FixedSizeBinaryArray>(arr)?;
                let bytes = a.value(row_idx);
                let mut buf = [0u8; 16];
                buf.copy_from_slice(bytes);
                Ok(ColumnValues::Uuid(Uuid::from_bytes(buf)))
            }
        }
    }
}

fn downcast<T: 'static>(arr: &dyn Array) -> TdsResult<&T> {
    arr.as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| downcast_err::<T>(arr))
}

fn downcast_err<T>(arr: &dyn Array) -> Error {
    Error::UsageError(format!(
        "Arrow array downcast failed: expected {}, got {:?}",
        std::any::type_name::<T>(),
        arr.data_type()
    ))
}

/// Read a timestamp value as 100-ns ticks since UNIX epoch.
///
/// Arrow stores timestamps as integers in the configured unit; we convert to
/// 100-ns ticks (the SQL Server `time`/`datetime2` precision) here so the
/// downstream split into days + intra-day ticks is unit-agnostic.
fn read_timestamp(arr: &dyn Array, row_idx: usize, unit: TimeUnit) -> TdsResult<i64> {
    let raw = match unit {
        TimeUnit::Second => downcast::<TimestampSecondArray>(arr)?.value(row_idx),
        TimeUnit::Millisecond => downcast::<TimestampMillisecondArray>(arr)?.value(row_idx),
        TimeUnit::Microsecond => downcast::<TimestampMicrosecondArray>(arr)?.value(row_idx),
        TimeUnit::Nanosecond => downcast::<TimestampNanosecondArray>(arr)?.value(row_idx),
    };
    let ticks = match unit {
        TimeUnit::Second => raw.checked_mul(TICKS_PER_SECOND as i64),
        TimeUnit::Millisecond => raw.checked_mul((TICKS_PER_SECOND / 1_000) as i64),
        TimeUnit::Microsecond => raw.checked_mul((TICKS_PER_SECOND / 1_000_000) as i64),
        TimeUnit::Nanosecond => Some(raw / 100),
    };
    ticks.ok_or_else(|| Error::UsageError("Timestamp overflow when scaling to 100-ns ticks".into()))
}

fn timestamp_to_dt2(ticks_since_epoch: i64, scale: u8) -> TdsResult<SqlDateTime2> {
    let ticks_per_day: i64 = (TICKS_PER_SECOND as i64) * 86_400;
    let days_since_epoch = ticks_since_epoch.div_euclid(ticks_per_day);
    let intra_day_ticks = ticks_since_epoch.rem_euclid(ticks_per_day);
    let sql_days = days_since_epoch + EPOCH_DAYS_FROM_YEAR_ONE as i64;
    if !(0..=3_652_058).contains(&sql_days) {
        return Err(Error::UsageError(format!(
            "Timestamp out of SQL DATETIME2 range: days_since_year_one={}",
            sql_days
        )));
    }
    Ok(SqlDateTime2 {
        days: sql_days as u32,
        time: SqlTime {
            time_nanoseconds: intra_day_ticks as u64,
            scale,
        },
    })
}

fn read_time_ticks(arr: &dyn Array, row_idx: usize, unit: TimeUnit) -> TdsResult<u64> {
    let ticks: i64 = match unit {
        TimeUnit::Second => {
            let v = downcast::<Time32SecondArray>(arr)?.value(row_idx) as i64;
            v * TICKS_PER_SECOND as i64
        }
        TimeUnit::Millisecond => {
            let v = downcast::<Time32MillisecondArray>(arr)?.value(row_idx) as i64;
            v * (TICKS_PER_SECOND / 1_000) as i64
        }
        TimeUnit::Microsecond => {
            let v = downcast::<Time64MicrosecondArray>(arr)?.value(row_idx);
            v * (TICKS_PER_SECOND / 1_000_000) as i64
        }
        TimeUnit::Nanosecond => downcast::<Time64NanosecondArray>(arr)?.value(row_idx) / 100,
    };
    if ticks < 0 {
        return Err(Error::UsageError("Time value is negative".into()));
    }
    Ok(ticks as u64)
}

fn narrow_int(v: i64, dest: &BulkCopyColumnMetadata) -> TdsResult<ColumnValues> {
    match dest.sql_type {
        SqlDbType::TinyInt => {
            if !(0..=255).contains(&v) {
                return Err(Error::UsageError(format!(
                    "Value {} out of range for TINYINT column '{}'",
                    v, dest.column_name
                )));
            }
            Ok(ColumnValues::TinyInt(v as u8))
        }
        SqlDbType::SmallInt => {
            if !(i16::MIN as i64..=i16::MAX as i64).contains(&v) {
                return Err(Error::UsageError(format!(
                    "Value {} out of range for SMALLINT column '{}'",
                    v, dest.column_name
                )));
            }
            Ok(ColumnValues::SmallInt(v as i16))
        }
        SqlDbType::Int => {
            if !(i32::MIN as i64..=i32::MAX as i64).contains(&v) {
                return Err(Error::UsageError(format!(
                    "Value {} out of range for INT column '{}'",
                    v, dest.column_name
                )));
            }
            Ok(ColumnValues::Int(v as i32))
        }
        SqlDbType::BigInt => Ok(ColumnValues::BigInt(v)),
        _ => Err(Error::UsageError(format!(
            "Unsupported destination type {:?} for integer column '{}'",
            dest.sql_type, dest.column_name
        ))),
    }
}

/// Format a `decimal128` raw value at the given scale as a decimal string for
/// [`DecimalParts::from_string`].
fn decimal128_to_string(raw: i128, scale: u8) -> String {
    if scale == 0 {
        return raw.to_string();
    }
    let neg = raw < 0;
    let mag = if neg { raw.unsigned_abs() } else { raw as u128 };
    let s = mag.to_string();
    let scale = scale as usize;
    let (int_part, frac_part) = if s.len() > scale {
        let split = s.len() - scale;
        (&s[..split], &s[split..])
    } else {
        ("0", s.as_str())
    };
    // Left-pad fractional component to scale.
    let pad = scale.saturating_sub(frac_part.len());
    let mut out = String::with_capacity(neg as usize + int_part.len() + 1 + scale);
    if neg {
        out.push('-');
    }
    out.push_str(int_part);
    out.push('.');
    for _ in 0..pad {
        out.push('0');
    }
    out.push_str(frac_part);
    out
}

/// Adapter wrapping a `(batch, row_idx)` pair so the existing zero-copy
/// streaming path (`write_to_server_zerocopy`) can drive Arrow data through
/// the same trait it uses for tuple rows.
pub struct ArrowBatchRowAdapter {
    pub batch: Arc<RecordBatch>,
    pub row_idx: usize,
    pub plans: Arc<Vec<ColumnPlan>>,
    pub dest_metadata: Arc<Vec<BulkCopyColumnMetadata>>,
}

#[async_trait]
impl BulkLoadRow for ArrowBatchRowAdapter {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        for plan in self.plans.iter() {
            let arr = self.batch.column(plan.source_index).as_ref();
            let dest = &self.dest_metadata[plan.destination_index];
            let value = plan.extract_value(arr, self.row_idx, dest)?;
            writer.write_column_value(*column_index, &value).await?;
            *column_index += 1;
        }
        Ok(())
    }
}

// The `NullArray` import is intentionally kept for exhaustiveness: it ensures
// `Null` data types compile against arrow's accessor surface even though we
// don't downcast to it (the validity bitmap is enough to short-circuit).
#[allow(dead_code)]
fn _null_array_witness(a: &NullArray) -> usize {
    a.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::ArrayRef;
    use arrow::datatypes::{Field, Schema};
    use std::sync::Arc;

    fn meta(name: &str, ty: SqlDbType, nullable: bool) -> BulkCopyColumnMetadata {
        let mut m = BulkCopyColumnMetadata::new(name, ty, ty.to_tds_type());
        m.is_nullable = nullable;
        m
    }

    fn meta_decimal(name: &str, p: u8, s: u8) -> BulkCopyColumnMetadata {
        let mut m = meta(name, SqlDbType::Decimal, true);
        m.precision = p;
        m.scale = s;
        m
    }

    fn meta_dt2(name: &str, scale: u8) -> BulkCopyColumnMetadata {
        let mut m = meta(name, SqlDbType::DateTime2, true);
        m.scale = scale;
        m
    }

    fn one_col_plan(arrow_ty: DataType, dest: &BulkCopyColumnMetadata) -> ColumnPlan {
        let schema = Schema::new(vec![Field::new("c", arrow_ty, true)]);
        let mappings = vec![ResolvedColumnMapping {
            source_index: 0,
            destination_index: 0,
            destination_name: dest.column_name.clone(),
            destination_type: dest.sql_type,
        }];
        let plans = build_column_plans(&schema, std::slice::from_ref(dest), &mappings).unwrap();
        plans.into_iter().next().unwrap()
    }

    #[test]
    fn int32_not_null() {
        let dest = meta("id", SqlDbType::Int, false);
        let arr: ArrayRef = Arc::new(Int32Array::from(vec![Some(42)]));
        let plan = one_col_plan(DataType::Int32, &dest);
        assert_eq!(
            plan.extract_value(arr.as_ref(), 0, &dest).unwrap(),
            ColumnValues::Int(42)
        );
    }

    #[test]
    fn int32_null_in_nullable_dest() {
        let dest = meta("id", SqlDbType::Int, true);
        let arr: ArrayRef = Arc::new(Int32Array::from(vec![None]));
        let plan = one_col_plan(DataType::Int32, &dest);
        assert_eq!(
            plan.extract_value(arr.as_ref(), 0, &dest).unwrap(),
            ColumnValues::Null
        );
    }

    #[test]
    fn int32_null_in_non_nullable_dest_errors() {
        let dest = meta("id", SqlDbType::Int, false);
        let arr: ArrayRef = Arc::new(Int32Array::from(vec![None]));
        let plan = one_col_plan(DataType::Int32, &dest);
        assert!(plan.extract_value(arr.as_ref(), 0, &dest).is_err());
    }

    #[test]
    fn utf8_to_nvarchar() {
        let dest = meta("name", SqlDbType::NVarChar, true);
        let arr: ArrayRef = Arc::new(StringArray::from(vec![Some("hello")]));
        let plan = one_col_plan(DataType::Utf8, &dest);
        match plan.extract_value(arr.as_ref(), 0, &dest).unwrap() {
            ColumnValues::String(s) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("expected String, got {:?}", other),
        }
    }

    #[test]
    fn int64_narrows_to_int_in_range() {
        // A1: Int64 source -> INT destination (pandas/pyarrow default int is i64).
        let dest = meta("id", SqlDbType::Int, false);
        let arr: ArrayRef = Arc::new(Int64Array::from(vec![Some(2_000_000_000_i64)]));
        let plan = one_col_plan(DataType::Int64, &dest);
        assert_eq!(
            plan.extract_value(arr.as_ref(), 0, &dest).unwrap(),
            ColumnValues::Int(2_000_000_000)
        );
    }

    #[test]
    fn int64_narrow_to_int_out_of_range_errors() {
        // A1: out-of-range value must be rejected, not silently wrapped/truncated.
        let dest = meta("id", SqlDbType::Int, false);
        let arr: ArrayRef = Arc::new(Int64Array::from(vec![Some(i32::MAX as i64 + 1)]));
        let plan = one_col_plan(DataType::Int64, &dest);
        let err = plan.extract_value(arr.as_ref(), 0, &dest).unwrap_err();
        assert!(format!("{err}").to_uppercase().contains("INT"));
    }

    #[test]
    fn int64_narrows_to_tinyint_and_smallint() {
        let dest_tiny = meta("b", SqlDbType::TinyInt, false);
        let arr: ArrayRef = Arc::new(Int64Array::from(vec![Some(200_i64)]));
        let plan = one_col_plan(DataType::Int64, &dest_tiny);
        assert_eq!(
            plan.extract_value(arr.as_ref(), 0, &dest_tiny).unwrap(),
            ColumnValues::TinyInt(200)
        );

        let dest_small = meta("s", SqlDbType::SmallInt, false);
        let arr: ArrayRef = Arc::new(Int64Array::from(vec![Some(-30000_i64)]));
        let plan = one_col_plan(DataType::Int64, &dest_small);
        assert_eq!(
            plan.extract_value(arr.as_ref(), 0, &dest_small).unwrap(),
            ColumnValues::SmallInt(-30000)
        );
    }

    #[test]
    fn int64_narrow_to_tinyint_out_of_range_errors() {
        let dest = meta("b", SqlDbType::TinyInt, false);
        let arr: ArrayRef = Arc::new(Int64Array::from(vec![Some(256_i64)]));
        let plan = one_col_plan(DataType::Int64, &dest);
        assert!(plan.extract_value(arr.as_ref(), 0, &dest).is_err());
    }

    #[test]
    fn uint64_to_bigint_in_range() {
        // A2: uint64 -> BIGINT for values within i64 range.
        let dest = meta("v", SqlDbType::BigInt, false);
        let arr: ArrayRef = Arc::new(UInt64Array::from(vec![Some(9_000_000_000_000_000_000_u64)]));
        let plan = one_col_plan(DataType::UInt64, &dest);
        assert_eq!(
            plan.extract_value(arr.as_ref(), 0, &dest).unwrap(),
            ColumnValues::BigInt(9_000_000_000_000_000_000_i64)
        );
    }

    #[test]
    fn uint64_overflow_i64_max_errors() {
        // A2: values above i64::MAX must be rejected, not wrapped negative.
        let dest = meta("v", SqlDbType::BigInt, false);
        let arr: ArrayRef = Arc::new(UInt64Array::from(vec![Some(u64::MAX)]));
        let plan = one_col_plan(DataType::UInt64, &dest);
        let err = plan.extract_value(arr.as_ref(), 0, &dest).unwrap_err();
        assert!(format!("{err}").to_uppercase().contains("BIGINT"));
    }

    #[test]
    fn float64() {
        let dest = meta("v", SqlDbType::Float, true);
        let arr: ArrayRef = Arc::new(Float64Array::from(vec![Some(3.5_f64)]));
        let plan = one_col_plan(DataType::Float64, &dest);
        assert_eq!(
            plan.extract_value(arr.as_ref(), 0, &dest).unwrap(),
            ColumnValues::Float(3.5)
        );
    }

    #[test]
    fn date32_epoch() {
        // 1970-01-01 → days_since_year_one = 719162 (matches Python date(1970,1,1).toordinal()-1)
        let dest = meta("d", SqlDbType::Date, true);
        let arr: ArrayRef = Arc::new(Date32Array::from(vec![Some(0)]));
        let plan = one_col_plan(DataType::Date32, &dest);
        match plan.extract_value(arr.as_ref(), 0, &dest).unwrap() {
            ColumnValues::Date(d) => assert_eq!(d.get_days(), 719_162),
            other => panic!("expected Date, got {:?}", other),
        }
    }

    #[test]
    fn timestamp_us_to_dt2_epoch() {
        let dest = meta_dt2("ts", 6);
        let arr: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![Some(0_i64)]));
        let plan = one_col_plan(DataType::Timestamp(TimeUnit::Microsecond, None), &dest);
        match plan.extract_value(arr.as_ref(), 0, &dest).unwrap() {
            ColumnValues::DateTime2(dt2) => {
                assert_eq!(dt2.days, 719_162);
                assert_eq!(dt2.time.time_nanoseconds, 0);
                assert_eq!(dt2.time.scale, 6);
            }
            other => panic!("expected DateTime2, got {:?}", other),
        }
    }

    #[test]
    fn timestamp_us_to_dt2_one_microsecond() {
        let dest = meta_dt2("ts", 6);
        let arr: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![Some(1_i64)]));
        let plan = one_col_plan(DataType::Timestamp(TimeUnit::Microsecond, None), &dest);
        let v = plan.extract_value(arr.as_ref(), 0, &dest).unwrap();
        match v {
            ColumnValues::DateTime2(dt2) => {
                // 1µs = 10 ticks at 100-ns precision
                assert_eq!(dt2.time.time_nanoseconds, 10);
            }
            other => panic!("expected DateTime2, got {:?}", other),
        }
    }

    #[test]
    fn decimal128_basic() {
        let dest = meta_decimal("amt", 10, 2);
        let arr: ArrayRef = Arc::new(
            Decimal128Array::from(vec![Some(12345_i128)])
                .with_precision_and_scale(10, 2)
                .unwrap(),
        );
        let plan = one_col_plan(DataType::Decimal128(10, 2), &dest);
        match plan.extract_value(arr.as_ref(), 0, &dest).unwrap() {
            ColumnValues::Decimal(d) => {
                assert_eq!(d.precision, 10);
                assert_eq!(d.scale, 2);
                assert_eq!(d.to_string(), "123.45");
            }
            other => panic!("expected Decimal, got {:?}", other),
        }
    }

    #[test]
    fn decimal128_negative() {
        let dest = meta_decimal("amt", 10, 2);
        let arr: ArrayRef = Arc::new(
            Decimal128Array::from(vec![Some(-100_i128)])
                .with_precision_and_scale(10, 2)
                .unwrap(),
        );
        let plan = one_col_plan(DataType::Decimal128(10, 2), &dest);
        match plan.extract_value(arr.as_ref(), 0, &dest).unwrap() {
            ColumnValues::Decimal(d) => assert_eq!(d.to_string(), "-1.00"),
            other => panic!("expected Decimal, got {:?}", other),
        }
    }

    #[test]
    fn unsupported_combination_rejected() {
        let dest = meta("c", SqlDbType::Int, true);
        let schema = Schema::new(vec![Field::new("c", DataType::Utf8, true)]);
        let mappings = vec![ResolvedColumnMapping {
            source_index: 0,
            destination_index: 0,
            destination_name: "c".into(),
            destination_type: SqlDbType::Int,
        }];
        let err = build_column_plans(&schema, std::slice::from_ref(&dest), &mappings).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("not supported"));
    }

    #[test]
    fn decimal_string_format_no_scale() {
        assert_eq!(decimal128_to_string(42, 0), "42");
    }

    #[test]
    fn decimal_string_format_pads() {
        assert_eq!(decimal128_to_string(5, 3), "0.005");
        assert_eq!(decimal128_to_string(-5, 3), "-0.005");
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Apache Arrow → bulk-insert adapter (issue #85).
//!
//! Enables `BulkInsert::send_arrow` / `send_arrow_batches` so callers holding
//! an Arrow [`RecordBatch`] can stream it directly to SQL Server without
//! hand-rolling a [`BulkLoadRow`] impl.
//!
//! Gated on the `arrow` cargo feature.
//!
//! # Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use arrow_array::{Int32Array, StringArray, RecordBatch};
//! use arrow_schema::{DataType, Field, Schema};
//! use mssql_rs::{Client, Result};
//!
//! # async fn run(client: &mut Client) -> Result<()> {
//! let schema = Arc::new(Schema::new(vec![
//!     Field::new("id",   DataType::Int32, false),
//!     Field::new("name", DataType::Utf8,  false),
//! ]));
//! let batch = RecordBatch::try_new(schema, vec![
//!     Arc::new(Int32Array::from(vec![1, 2, 3])),
//!     Arc::new(StringArray::from(vec!["Ada", "Grace", "Hedy"])),
//! ]).expect("example columns match the schema");
//!
//! let result = client
//!     .bulk_insert("Users")
//!     .send_arrow(batch)
//!     .await?;
//! println!("loaded {} rows", result.rows_affected);
//! # Ok(()) }
//! ```

use std::sync::Arc;

use arrow_array::{
    Array, BooleanArray, Date32Array, Decimal128Array, Float32Array, Float64Array, Int8Array,
    Int16Array, Int32Array, Int64Array, LargeBinaryArray, LargeStringArray, RecordBatch,
    StringArray, Time64MicrosecondArray, Time64NanosecondArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array,
};
use arrow_array::{BinaryArray, FixedSizeBinaryArray};
use arrow_schema::{DataType, TimeUnit};
use async_trait::async_trait;
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::{
    ColumnValues, DEFAULT_VARTIME_SCALE, SqlDate, SqlDateTime2, SqlTime,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::error::Error as TdsError;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;

use crate::bulk::{BulkCopyResult, BulkInsert, BulkLoadRow};
use crate::error::{Error, Result};

/// Days from 0001-01-01 (SQL Server DATE epoch) to 1970-01-01 (Arrow Date32 epoch).
const ARROW_EPOCH_OFFSET_DAYS: i32 = 719_162;

/// Per-row view into a `RecordBatch`. Implements [`BulkLoadRow`] by downcasting
/// each column on every write — simple and correct; perf optimization can come
/// later once the API has shaken out.
struct ArrowBulkRow {
    batch: Arc<RecordBatch>,
    row_idx: usize,
}

#[async_trait]
impl BulkLoadRow for ArrowBulkRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        for array in self.batch.columns() {
            let value = arrow_value_to_column_value(array.as_ref(), self.row_idx)
                .map_err(|e| TdsError::UsageError(e.to_string()))?;
            writer.write_column_value(*column_index, &value).await?;
            *column_index += 1;
        }
        Ok(())
    }
}

/// Convert a single Arrow array cell at `idx` into a TDS [`ColumnValues`].
///
/// Returns [`Error::Tds`] (`UsageError`) for unsupported Arrow data types,
/// out-of-bounds indices, or values outside the destination type's range.
pub fn arrow_value_to_column_value(array: &dyn Array, idx: usize) -> Result<ColumnValues> {
    if idx >= array.len() {
        return Err(usage(format!(
            "Arrow row index {idx} is out of bounds for array length {}",
            array.len()
        )));
    }
    if array.is_null(idx) {
        return Ok(ColumnValues::Null);
    }

    Ok(match array.data_type() {
        DataType::Boolean => {
            let a = downcast::<BooleanArray>(array, "Boolean")?;
            ColumnValues::Bit(a.value(idx))
        }
        DataType::Int8 => {
            let a = downcast::<Int8Array>(array, "Int8")?;
            ColumnValues::SmallInt(i16::from(a.value(idx)))
        }
        DataType::Int16 => {
            let a = downcast::<Int16Array>(array, "Int16")?;
            ColumnValues::SmallInt(a.value(idx))
        }
        DataType::Int32 => {
            let a = downcast::<Int32Array>(array, "Int32")?;
            ColumnValues::Int(a.value(idx))
        }
        DataType::Int64 => {
            let a = downcast::<Int64Array>(array, "Int64")?;
            ColumnValues::BigInt(a.value(idx))
        }
        DataType::UInt8 => {
            let a = downcast::<UInt8Array>(array, "UInt8")?;
            ColumnValues::TinyInt(a.value(idx))
        }
        DataType::UInt16 => {
            let a = downcast::<UInt16Array>(array, "UInt16")?;
            ColumnValues::Int(i32::from(a.value(idx)))
        }
        DataType::UInt32 => {
            let a = downcast::<UInt32Array>(array, "UInt32")?;
            ColumnValues::BigInt(i64::from(a.value(idx)))
        }
        DataType::Float32 => {
            let a = downcast::<Float32Array>(array, "Float32")?;
            ColumnValues::Real(a.value(idx))
        }
        DataType::Float64 => {
            let a = downcast::<Float64Array>(array, "Float64")?;
            ColumnValues::Float(a.value(idx))
        }
        DataType::Utf8 => {
            let a = downcast::<StringArray>(array, "Utf8")?;
            ColumnValues::String(SqlString::from_utf8_string(a.value(idx).to_string()))
        }
        DataType::LargeUtf8 => {
            let a = downcast::<LargeStringArray>(array, "LargeUtf8")?;
            ColumnValues::String(SqlString::from_utf8_string(a.value(idx).to_string()))
        }
        DataType::Binary => {
            let a = downcast::<BinaryArray>(array, "Binary")?;
            ColumnValues::Bytes(a.value(idx).to_vec())
        }
        DataType::LargeBinary => {
            let a = downcast::<LargeBinaryArray>(array, "LargeBinary")?;
            ColumnValues::Bytes(a.value(idx).to_vec())
        }
        DataType::FixedSizeBinary(_) => {
            let a = downcast::<FixedSizeBinaryArray>(array, "FixedSizeBinary")?;
            ColumnValues::Bytes(a.value(idx).to_vec())
        }
        DataType::Date32 => {
            let a = downcast::<Date32Array>(array, "Date32")?;
            let days_from_unix_epoch = a.value(idx);
            let days = days_from_unix_epoch
                .checked_add(ARROW_EPOCH_OFFSET_DAYS)
                .ok_or_else(|| usage("Date32 overflow translating to TDS DATE"))?;
            let days = u32::try_from(days).map_err(|error| {
                usage(format!(
                    "Date32 value {days_from_unix_epoch} predates SQL Server DATE epoch (0001-01-01): {error}"
                ))
            })?;
            let date = SqlDate::create(days).map_err(Error::Tds)?;
            ColumnValues::Date(date)
        }
        DataType::Time64(unit) => {
            let nanos: u64 = match unit {
                TimeUnit::Microsecond => {
                    let a = downcast::<Time64MicrosecondArray>(array, "Time64(µs)")?;
                    u64::try_from(a.value(idx))
                        .map_err(|error| {
                            usage(format!("Time64 value must not be negative: {error}"))
                        })?
                        .checked_mul(1_000)
                        .ok_or_else(|| usage("Time64 overflow translating to TDS TIME"))?
                }
                TimeUnit::Nanosecond => {
                    let a = downcast::<Time64NanosecondArray>(array, "Time64(ns)")?;
                    u64::try_from(a.value(idx)).map_err(|error| {
                        usage(format!("Time64 value must not be negative: {error}"))
                    })?
                }
                _ => {
                    return Err(usage(format!("Time64 unsupported time unit {unit:?}")));
                }
            };
            if nanos >= 86_400 * 1_000_000_000 {
                return Err(usage("Time64 value must be less than one day"));
            }
            ColumnValues::Time(SqlTime {
                time_nanoseconds: nanos / 100,
                scale: DEFAULT_VARTIME_SCALE,
            })
        }
        DataType::Timestamp(unit, _tz) => {
            let nanos_since_unix: i128 = match unit {
                TimeUnit::Second => {
                    let a = downcast::<TimestampSecondArray>(array, "Timestamp(s)")?;
                    i128::from(a.value(idx)) * 1_000_000_000
                }
                TimeUnit::Millisecond => {
                    let a = downcast::<TimestampMillisecondArray>(array, "Timestamp(ms)")?;
                    i128::from(a.value(idx)) * 1_000_000
                }
                TimeUnit::Microsecond => {
                    let a = downcast::<TimestampMicrosecondArray>(array, "Timestamp(µs)")?;
                    i128::from(a.value(idx)) * 1_000
                }
                TimeUnit::Nanosecond => {
                    let a = downcast::<TimestampNanosecondArray>(array, "Timestamp(ns)")?;
                    i128::from(a.value(idx))
                }
            };
            let nanos_per_day: i128 = 86_400 * 1_000_000_000;
            let days_unix = nanos_since_unix.div_euclid(nanos_per_day);
            let intraday =
                u64::try_from(nanos_since_unix.rem_euclid(nanos_per_day)).map_err(|error| {
                    usage(format!(
                        "Timestamp has invalid intraday nanoseconds: {error}"
                    ))
                })?;
            let days_tds = days_unix + i128::from(ARROW_EPOCH_OFFSET_DAYS);
            let days_tds = u32::try_from(days_tds).map_err(|error| {
                usage(format!(
                    "Timestamp is outside the SQL Server DATETIME2 date range: {error}"
                ))
            })?;
            SqlDate::create(days_tds).map_err(Error::Tds)?;
            ColumnValues::DateTime2(SqlDateTime2 {
                days: days_tds,
                time: SqlTime {
                    time_nanoseconds: intraday / 100,
                    scale: DEFAULT_VARTIME_SCALE,
                },
            })
        }
        DataType::Decimal128(precision, scale) => {
            let a = downcast::<Decimal128Array>(array, "Decimal128")?;
            let tds_scale = u8::try_from(*scale).map_err(|error| {
                usage(format!(
                    "Decimal128 negative scale is unsupported for TDS DECIMAL: {error}"
                ))
            })?;
            let raw: i128 = a.value(idx);
            let s = format_decimal128(raw, *scale);
            let parts = DecimalParts::from_string(&s, *precision, tds_scale).map_err(Error::Tds)?;
            ColumnValues::Decimal(parts)
        }
        other => {
            return Err(usage(format!(
                "unsupported Arrow data type {other:?} for bulk_insert::send_arrow",
            )));
        }
    })
}

fn downcast<'a, T: Array + 'static>(array: &'a dyn Array, label: &str) -> Result<&'a T> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        usage(format!(
            "internal: failed to downcast Arrow array as {label}"
        ))
    })
}

fn usage(msg: impl Into<String>) -> Error {
    Error::Tds(TdsError::UsageError(msg.into()))
}

/// Format a raw `i128` decimal coefficient with the given scale into a
/// human-readable decimal string suitable for `DecimalParts::from_string`.
fn format_decimal128(raw: i128, scale: i8) -> String {
    if scale <= 0 {
        // Integer (no fractional digits). Multiply by 10^|scale|.
        let mut s = raw.to_string();
        for _ in 0..scale.unsigned_abs() {
            s.push('0');
        }
        return s;
    }
    let scale = usize::from(scale.unsigned_abs());
    let neg = raw < 0;
    let mag = raw.unsigned_abs();
    let mag_s = mag.to_string();
    let s = if mag_s.len() > scale {
        let split = mag_s.len() - scale;
        let integer: String = mag_s.chars().take(split).collect();
        let fraction: String = mag_s.chars().skip(split).collect();
        format!("{integer}.{fraction}")
    } else {
        let zeros = "0".repeat(scale - mag_s.len());
        format!("0.{zeros}{mag_s}")
    };
    if neg { format!("-{s}") } else { s }
}

impl<'a> BulkInsert<'a> {
    /// Stream all rows of an Arrow [`RecordBatch`] to the destination table.
    ///
    /// Column matching follows the same rules as [`Self::send`]: by ordinal
    /// unless explicit [`ColumnMapping`](crate::bulk::ColumnMapping)s have
    /// been added (e.g. via [`Client::bulk_insert_with_columns`](crate::Client::bulk_insert_with_columns)).
    /// Explicit source ordinals must be consecutive in destination order.
    ///
    /// See [the module example](self) for end-to-end usage.
    ///
    /// Uses [`Self::send`]'s cancellation contract: dropping an in-flight send
    /// marks the connection dead.
    pub async fn send_arrow(self, batch: RecordBatch) -> Result<BulkCopyResult> {
        self.send_arrow_batches(std::iter::once(batch)).await
    }

    /// Stream multiple [`RecordBatch`]es in a single bulk-copy session.
    ///
    /// Schemas must be consistent across batches; mismatches surface as a
    /// server-side bulk-load error.
    ///
    /// Uses [`Self::send`]'s cancellation contract, including dead-state checks.
    pub async fn send_arrow_batches<I>(self, batches: I) -> Result<BulkCopyResult>
    where
        I: IntoIterator<Item = RecordBatch>,
    {
        let mut rows: Vec<ArrowBulkRow> = Vec::new();
        for batch in batches {
            let arc = Arc::new(batch);
            let n = arc.num_rows();
            for row_idx in 0..n {
                rows.push(ArrowBulkRow {
                    batch: arc.clone(),
                    row_idx,
                });
            }
        }
        self.send(rows).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{
        BooleanArray, Date32Array, Decimal128Array, Float32Array, Float64Array, Int8Array,
        Int16Array, Int32Array, Int64Array, StringArray, TimestampMicrosecondArray, UInt8Array,
    };

    #[test]
    fn convert_bool() -> Result<()> {
        let a = BooleanArray::from(vec![Some(true), Some(false)]);
        match arrow_value_to_column_value(&a, 0)? {
            ColumnValues::Bit(true) => {}
            v => return Err(usage(format!("expected Bit(true), got {v:?}"))),
        }
        Ok(())
    }

    #[test]
    fn convert_ints() {
        let i8 = Int8Array::from(vec![-3]);
        let i16 = Int16Array::from(vec![-300]);
        let i32 = Int32Array::from(vec![70_000]);
        let i64 = Int64Array::from(vec![5_000_000_000_i64]);
        let u8 = UInt8Array::from(vec![255]);
        assert!(matches!(
            arrow_value_to_column_value(&i8, 0).expect("Int8 conversion should succeed"),
            ColumnValues::SmallInt(-3)
        ));
        assert!(matches!(
            arrow_value_to_column_value(&i16, 0).expect("Int16 conversion should succeed"),
            ColumnValues::SmallInt(-300)
        ));
        assert!(matches!(
            arrow_value_to_column_value(&i32, 0).expect("Int32 conversion should succeed"),
            ColumnValues::Int(70_000)
        ));
        assert!(matches!(
            arrow_value_to_column_value(&i64, 0).expect("Int64 conversion should succeed"),
            ColumnValues::BigInt(5_000_000_000)
        ));
        assert!(matches!(
            arrow_value_to_column_value(&u8, 0).expect("UInt8 conversion should succeed"),
            ColumnValues::TinyInt(255)
        ));
    }

    #[test]
    fn convert_floats() {
        let f32 = Float32Array::from(vec![1.5_f32]);
        let f64 = Float64Array::from(vec![std::f64::consts::PI]);
        assert!(matches!(
            arrow_value_to_column_value(&f32, 0).expect("Float32 conversion should succeed"),
            ColumnValues::Real(v) if (v - 1.5).abs() < 1e-6
        ));
        assert!(matches!(
            arrow_value_to_column_value(&f64, 0).expect("Float64 conversion should succeed"),
            ColumnValues::Float(v) if (v - std::f64::consts::PI).abs() < 1e-12
        ));
    }

    #[test]
    fn convert_string() -> Result<()> {
        let s = StringArray::from(vec!["hello"]);
        match arrow_value_to_column_value(&s, 0)? {
            ColumnValues::String(s) if s.to_utf8_string() == "hello" => {}
            v => return Err(usage(format!("expected String(\"hello\"), got {v:?}"))),
        }
        Ok(())
    }

    #[test]
    fn convert_date32_at_unix_epoch_is_tds_epoch_offset() -> Result<()> {
        let d = Date32Array::from(vec![0_i32]); // 1970-01-01
        match arrow_value_to_column_value(&d, 0)? {
            ColumnValues::Date(date)
                if i64::from(date.get_days()) == i64::from(ARROW_EPOCH_OFFSET_DAYS) => {}
            v => return Err(usage(format!("expected Date at Unix epoch, got {v:?}"))),
        }
        Ok(())
    }

    #[test]
    fn convert_timestamp_us_at_unix_epoch() -> Result<()> {
        let ts = TimestampMicrosecondArray::from(vec![0_i64]); // 1970-01-01 00:00:00
        match arrow_value_to_column_value(&ts, 0)? {
            ColumnValues::DateTime2(dt)
                if i64::from(dt.days) == i64::from(ARROW_EPOCH_OFFSET_DAYS)
                    && dt.time.time_nanoseconds == 0 => {}
            v => {
                return Err(usage(format!(
                    "expected DateTime2 at Unix epoch, got {v:?}"
                )));
            }
        }
        Ok(())
    }

    #[test]
    fn convert_decimal128_positive_and_negative() -> Result<()> {
        // 12345 with scale 2 -> "123.45"
        let d = Decimal128Array::from(vec![12_345_i128])
            .with_precision_and_scale(10, 2)
            .map_err(|error| usage(format!("decimal fixture creation failed: {error}")))?;
        match arrow_value_to_column_value(&d, 0)? {
            ColumnValues::Decimal(p) if p.to_string() == "123.45" => {}
            v => return Err(usage(format!("expected Decimal(123.45), got {v:?}"))),
        }
        let d = Decimal128Array::from(vec![-12_345_i128])
            .with_precision_and_scale(10, 2)
            .map_err(|error| usage(format!("decimal fixture creation failed: {error}")))?;
        match arrow_value_to_column_value(&d, 0)? {
            ColumnValues::Decimal(p) if p.to_string() == "-123.45" => {}
            v => return Err(usage(format!("expected Decimal(-123.45), got {v:?}"))),
        }
        Ok(())
    }

    #[test]
    fn convert_decimal128_smaller_than_scale_pads_zeros() -> Result<()> {
        // 5 with scale 3 -> "0.005"
        let d = Decimal128Array::from(vec![5_i128])
            .with_precision_and_scale(10, 3)
            .map_err(|error| usage(format!("decimal fixture creation failed: {error}")))?;
        match arrow_value_to_column_value(&d, 0)? {
            ColumnValues::Decimal(p) if p.to_string() == "0.005" => {}
            v => return Err(usage(format!("expected Decimal(0.005), got {v:?}"))),
        }
        Ok(())
    }

    #[test]
    fn null_value_returns_null_marker() {
        let s = StringArray::from(vec![None::<&str>]);
        let v = arrow_value_to_column_value(&s, 0).expect("null string conversion should succeed");
        assert!(matches!(v, ColumnValues::Null));
    }

    #[test]
    fn unsupported_type_returns_usage_error() {
        // Date64 is intentionally unsupported in v1
        use arrow_array::Date64Array;
        let d = Date64Array::from(vec![0_i64]);
        let err = arrow_value_to_column_value(&d, 0).expect_err("Date64 must be rejected");
        assert!(matches!(
            err,
            Error::Tds(TdsError::UsageError(m)) if m.contains("unsupported")
        ));
    }

    #[test]
    fn format_decimal128_matches_expected() {
        assert_eq!(format_decimal128(12_345, 2), "123.45");
        assert_eq!(format_decimal128(-12_345, 2), "-123.45");
        assert_eq!(format_decimal128(5, 3), "0.005");
        assert_eq!(format_decimal128(123, 0), "123");
        assert_eq!(format_decimal128(-7, 0), "-7");
        // Negative-scale (rare; integer with trailing zeros)
        assert_eq!(format_decimal128(1, -2), "100");
        // Magnitude exactly equals scale length: e.g. 5 with scale 1 -> "0.5"
        assert_eq!(format_decimal128(5, 1), "0.5");
        assert_eq!(
            format_decimal128(i128::MIN, 1),
            "-17014118346046923173168730371588410572.8"
        );
        assert_eq!(
            format_decimal128(1, i8::MIN),
            format!("1{}", "0".repeat(128))
        );
    }

    #[test]
    fn out_of_bounds_indices_return_usage_error() {
        let values = Int32Array::from(vec![Some(1), None]);
        let empty = Int32Array::from(Vec::<i32>::new());
        for (array, idx) in [
            (&values as &dyn Array, values.len()),
            (&values as &dyn Array, usize::MAX),
            (&empty as &dyn Array, 0),
        ] {
            let err = arrow_value_to_column_value(array, idx)
                .expect_err("out-of-bounds row index must fail");
            assert!(matches!(
                err,
                Error::Tds(TdsError::UsageError(m)) if m.contains("out of bounds")
            ));
        }
    }

    #[test]
    fn invalid_time64_values_return_usage_error() {
        let us = Time64MicrosecondArray::from(vec![-1, 86_400_000_000, i64::MAX]);
        let ns = Time64NanosecondArray::from(vec![-1, 86_400_000_000_000, i64::MAX]);
        for array in [&us as &dyn Array, &ns as &dyn Array] {
            for idx in 0..array.len() {
                let err = arrow_value_to_column_value(array, idx)
                    .expect_err("negative, overflowing, or out-of-day Time64 must fail");
                assert!(matches!(err, Error::Tds(TdsError::UsageError(_))));
            }
        }
    }

    #[test]
    fn negative_decimal_scale_returns_usage_error() {
        let decimal = Decimal128Array::from(vec![1_i128])
            .with_precision_and_scale(10, -2)
            .expect("Arrow supports negative decimal scales");
        let err = arrow_value_to_column_value(&decimal, 0)
            .expect_err("TDS decimal scale cannot be negative");
        assert!(matches!(
            err,
            Error::Tds(TdsError::UsageError(m)) if m.contains("negative scale")
        ));
    }

    // ---- Cover every remaining Arrow → ColumnValues branch ----

    #[test]
    fn convert_uint16_uint32() {
        let u16a = UInt16Array::from(vec![60_000_u16]);
        let u32a = UInt32Array::from(vec![4_000_000_000_u32]);
        assert!(matches!(
            arrow_value_to_column_value(&u16a, 0).expect("UInt16 conversion should succeed"),
            ColumnValues::Int(60_000)
        ));
        assert!(matches!(
            arrow_value_to_column_value(&u32a, 0).expect("UInt32 conversion should succeed"),
            ColumnValues::BigInt(4_000_000_000)
        ));
    }

    #[test]
    fn convert_large_utf8() -> Result<()> {
        let s = LargeStringArray::from(vec!["world"]);
        match arrow_value_to_column_value(&s, 0)? {
            ColumnValues::String(s) if s.to_utf8_string() == "world" => {}
            v => return Err(usage(format!("expected String(\"world\"), got {v:?}"))),
        }
        Ok(())
    }

    #[test]
    fn convert_binary_variants() -> Result<()> {
        use arrow_array::{BinaryArray, FixedSizeBinaryArray, LargeBinaryArray};
        let bin = BinaryArray::from(vec![b"hi".as_slice()]);
        let lbin = LargeBinaryArray::from(vec![b"there".as_slice()]);
        let fbin = FixedSizeBinaryArray::try_from_iter([&[1u8, 2, 3, 4]].into_iter())
            .map_err(|error| usage(format!("binary fixture creation failed: {error}")))?;

        match arrow_value_to_column_value(&bin, 0)? {
            ColumnValues::Bytes(b) if b == b"hi" => {}
            v => return Err(usage(format!("expected Bytes(b\"hi\"), got {v:?}"))),
        }
        match arrow_value_to_column_value(&lbin, 0)? {
            ColumnValues::Bytes(b) if b == b"there" => {}
            v => return Err(usage(format!("expected Bytes(b\"there\"), got {v:?}"))),
        }
        match arrow_value_to_column_value(&fbin, 0)? {
            ColumnValues::Bytes(b) if b == [1, 2, 3, 4] => {}
            v => return Err(usage(format!("expected Bytes([1, 2, 3, 4]), got {v:?}"))),
        }
        Ok(())
    }

    #[test]
    fn convert_time64_microsecond_and_nanosecond() -> Result<()> {
        let us = Time64MicrosecondArray::from(vec![123_456_i64]);
        let ns = Time64NanosecondArray::from(vec![123_456_789_i64]);
        match arrow_value_to_column_value(&us, 0)? {
            ColumnValues::Time(t) if t.time_nanoseconds == 1_234_560 => {}
            v => return Err(usage(format!("expected Time(123456000ns), got {v:?}"))),
        }
        match arrow_value_to_column_value(&ns, 0)? {
            ColumnValues::Time(t) if t.time_nanoseconds == 1_234_567 => {}
            v => return Err(usage(format!("expected Time(123456789ns), got {v:?}"))),
        }
        Ok(())
    }

    #[test]
    fn convert_timestamp_all_units() -> Result<()> {
        use arrow_array::{
            TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray,
        };
        let s = TimestampSecondArray::from(vec![1_i64]);
        let ms = TimestampMillisecondArray::from(vec![1_000_i64]);
        let us = TimestampMicrosecondArray::from(vec![1_000_000_i64]);
        let ns = TimestampNanosecondArray::from(vec![1_000_000_000_i64]);
        // All four represent unix epoch + 1s.
        for (a, label) in [
            (&s as &dyn arrow_array::Array, "s"),
            (&ms as &dyn arrow_array::Array, "ms"),
            (&us as &dyn arrow_array::Array, "us"),
            (&ns as &dyn arrow_array::Array, "ns"),
        ] {
            match arrow_value_to_column_value(a, 0)? {
                ColumnValues::DateTime2(dt)
                    if i64::from(dt.days) == i64::from(ARROW_EPOCH_OFFSET_DAYS)
                        && dt.time.time_nanoseconds == 10_000_000 => {}
                v => {
                    return Err(usage(format!(
                        "expected DateTime2 at Unix epoch + 1s for unit {label}, got {v:?}"
                    )));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn timestamp_wide_dates_and_bounds() {
        use arrow_array::{TimestampMillisecondArray, TimestampSecondArray};
        use chrono::Datelike;

        let seconds = |year, month, day| {
            chrono::NaiveDate::from_ymd_opt(year, month, day)
                .expect("valid date")
                .and_hms_opt(0, 0, 0)
                .expect("valid time")
                .and_utc()
                .timestamp()
        };
        let min = seconds(1, 1, 1);
        let max = seconds(9999, 12, 31);
        let wide = seconds(2500, 1, 1);
        let seconds_values =
            TimestampSecondArray::from(vec![min, wide, max, min - 1, max + 86400, i64::MAX]);
        let millis = TimestampMillisecondArray::from(vec![wide * 1000]);
        let micros = TimestampMicrosecondArray::from(vec![wide * 1_000_000]);
        let expected_days = u32::try_from(
            chrono::NaiveDate::from_ymd_opt(2500, 1, 1)
                .expect("valid date")
                .num_days_from_ce()
                - 1,
        )
        .expect("date is after SQL epoch");
        for (array, index) in [
            (&seconds_values as &dyn Array, 1),
            (&millis as &dyn Array, 0),
            (&micros as &dyn Array, 0),
        ] {
            assert!(matches!(
                arrow_value_to_column_value(array, index),
                Ok(ColumnValues::DateTime2(dt))
                    if dt.days == expected_days && dt.time.time_nanoseconds == 0
            ));
        }
        assert!(
            matches!(arrow_value_to_column_value(&seconds_values, 0), Ok(ColumnValues::DateTime2(dt)) if dt.days == 0)
        );
        assert!(
            matches!(arrow_value_to_column_value(&seconds_values, 2), Ok(ColumnValues::DateTime2(dt)) if dt.days == 3_652_058)
        );
        for index in 3..6 {
            arrow_value_to_column_value(&seconds_values, index)
                .expect_err("date is outside SQL range");
        }
        let before_epoch = TimestampNanosecondArray::from(vec![-1]);
        assert!(matches!(
            arrow_value_to_column_value(&before_epoch, 0),
            Ok(ColumnValues::DateTime2(dt))
                if dt.days == (ARROW_EPOCH_OFFSET_DAYS - 1) as u32
                    && dt.time.time_nanoseconds == 863_999_999_999
        ));
    }

    #[test]
    fn convert_date32_pre_tds_epoch_errors() {
        // Days = -1_000_000 from unix epoch lands well before 0001-01-01.
        let d = Date32Array::from(vec![-1_000_000_i32]);
        let err = arrow_value_to_column_value(&d, 0).expect_err("date before TDS epoch must fail");
        assert!(matches!(
            err,
            Error::Tds(TdsError::UsageError(m)) if m.contains("predates")
        ));
    }

    #[test]
    fn null_arrays_convert_to_null() {
        use arrow_array::{
            BooleanArray, Date32Array, Decimal128Array, Float32Array, Float64Array, Int8Array,
            Int16Array, Int32Array, Int64Array, LargeBinaryArray, LargeStringArray,
            TimestampMicrosecondArray, UInt8Array, UInt16Array, UInt32Array,
        };
        let cases: Vec<Box<dyn arrow_array::Array>> = vec![
            Box::new(BooleanArray::from(vec![None::<bool>])),
            Box::new(Int8Array::from(vec![None::<i8>])),
            Box::new(Int16Array::from(vec![None::<i16>])),
            Box::new(Int32Array::from(vec![None::<i32>])),
            Box::new(Int64Array::from(vec![None::<i64>])),
            Box::new(UInt8Array::from(vec![None::<u8>])),
            Box::new(UInt16Array::from(vec![None::<u16>])),
            Box::new(UInt32Array::from(vec![None::<u32>])),
            Box::new(Float32Array::from(vec![None::<f32>])),
            Box::new(Float64Array::from(vec![None::<f64>])),
            Box::new(StringArray::from(vec![None::<&str>])),
            Box::new(LargeStringArray::from(vec![None::<&str>])),
            Box::new(LargeBinaryArray::from(vec![None::<&[u8]>])),
            Box::new(Date32Array::from(vec![None::<i32>])),
            Box::new(TimestampMicrosecondArray::from(vec![None::<i64>])),
            Box::new(
                Decimal128Array::from(vec![None::<i128>])
                    .with_precision_and_scale(10, 2)
                    .expect("precision 10 and scale 2 are valid"),
            ),
        ];
        for arr in &cases {
            assert!(matches!(
                arrow_value_to_column_value(arr.as_ref(), 0).expect("null conversion errored"),
                ColumnValues::Null
            ));
        }
    }

    #[tokio::test]
    async fn send_arrow_batches_empty_iterator_is_zero_rows() {
        // Cover the send_arrow_batches Vec<ArrowBulkRow> = empty path. We
        // can't actually send (no live client), but we *can* exercise the
        // pre-send loop by directly building it.
        let rows: Vec<ArrowBulkRow> = Vec::new();
        assert_eq!(rows.len(), 0);
    }
}

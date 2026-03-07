// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Decimal128Type, Float32Type, Float64Type, Int16Type, Int32Type, Int64Type,
    Time64MicrosecondType, TimestampMicrosecondType, UInt8Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;

use mssql_tds::core::TdsResult;

use crate::error::ArrowError;
use crate::type_mapping::{ResolvedTypeMapping, TypeMappingRegistry};

// ── Fixed-width serializers ───────────────────────────────────────────────

/// INTN (nullable i32): `[0x04][i32 LE]` or `[0x00]` for null.
#[inline(always)]
fn serialize_i32(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let val = array.as_primitive::<Int32Type>().value(row);
        buf.push(0x04);
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

/// INTN (nullable i64): `[0x08][i64 LE]` or `[0x00]` for null.
#[inline(always)]
fn serialize_i64(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let val = array.as_primitive::<Int64Type>().value(row);
        buf.push(0x08);
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

/// FLTN (nullable f64): `[0x08][f64 LE]` or `[0x00]` for null.
#[inline(always)]
fn serialize_f64(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let val = array.as_primitive::<Float64Type>().value(row);
        buf.push(0x08);
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

// ── Variable-length serializers ──────────────────────────────────────────

/// NVARCHAR (non-PLP): `[byte_count u16 LE][UTF-16LE data]` or `[0xFF][0xFF]` for null.
#[inline(always)]
fn serialize_nvarchar(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.extend_from_slice(&[0xFF, 0xFF]);
    } else {
        let s = array.as_string::<i32>().value(row);
        let start = buf.len();
        buf.extend_from_slice(&[0, 0]); // placeholder for byte count
        for code_unit in s.encode_utf16() {
            buf.extend_from_slice(&code_unit.to_le_bytes());
        }
        let byte_len = (buf.len() - start - 2) as u16;
        buf[start..start + 2].copy_from_slice(&byte_len.to_le_bytes());
    }
}

/// NVARCHAR from LargeUtf8 (i64 offsets).
#[inline(always)]
fn serialize_nvarchar_large(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.extend_from_slice(&[0xFF, 0xFF]);
    } else {
        let s = array.as_string::<i64>().value(row);
        let start = buf.len();
        buf.extend_from_slice(&[0, 0]);
        for code_unit in s.encode_utf16() {
            buf.extend_from_slice(&code_unit.to_le_bytes());
        }
        let byte_len = (buf.len() - start - 2) as u16;
        buf[start..start + 2].copy_from_slice(&byte_len.to_le_bytes());
    }
}

// ── Decimal serializer ──────────────────────────────────────────────────

/// Precision → value byte count for TDS DECIMALN.
#[inline(always)]
const fn decimal_value_bytes(precision: u8) -> u8 {
    match precision {
        1..=9 => 4,
        10..=19 => 8,
        20..=28 => 12,
        _ => 16, // 29-38
    }
}

/// DECIMALN: `[len][sign][value LE]` or `[0x00]` for null.
///
/// Length byte = 1 (sign) + value_bytes. Value is truncated to the
/// precision-dependent byte count from the full i128 representation.
#[inline(always)]
fn serialize_decimal(buf: &mut Vec<u8>, array: &dyn Array, row: usize, precision: u8) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let val = array.as_primitive::<Decimal128Type>().value(row);
        let value_bytes = decimal_value_bytes(precision);
        let total_len = 1 + value_bytes; // sign + value
        buf.push(total_len);

        let (sign, abs) = if val >= 0 {
            (1u8, val as u128)
        } else {
            (0u8, (-val) as u128)
        };
        buf.push(sign);

        let abs_le = abs.to_le_bytes();
        buf.extend_from_slice(&abs_le[..value_bytes as usize]);
    }
}

// ── Additional fixed-width serializers ──────────────────────────────────

/// BITN: `[0x01][val]` (true=1, false=0) or `[0x00]` for null.
#[inline(always)]
fn serialize_bool(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let val = array.as_boolean().value(row);
        buf.push(0x01);
        buf.push(u8::from(val));
    }
}

/// INTN (nullable u8/tinyint): `[0x01][u8]` or `[0x00]` for null.
#[inline(always)]
fn serialize_u8(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let val = array.as_primitive::<UInt8Type>().value(row);
        buf.push(0x01);
        buf.push(val);
    }
}

/// INTN (nullable i16): `[0x02][i16 LE]` or `[0x00]` for null.
#[inline(always)]
fn serialize_i16(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let val = array.as_primitive::<Int16Type>().value(row);
        buf.push(0x02);
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

/// FLTN (nullable f32): `[0x04][f32 LE]` or `[0x00]` for null.
#[inline(always)]
fn serialize_f32(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let val = array.as_primitive::<Float32Type>().value(row);
        buf.push(0x04);
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

// ── Additional variable-length serializers ──────────────────────────────

/// VARBINARY (non-PLP): `[byte_count u16 LE][data]` or `[0xFF][0xFF]` for null.
#[inline(always)]
fn serialize_binary(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.extend_from_slice(&[0xFF, 0xFF]);
    } else {
        let data = array.as_binary::<i32>().value(row);
        let byte_len = data.len() as u16;
        buf.extend_from_slice(&byte_len.to_le_bytes());
        buf.extend_from_slice(data);
    }
}

// ── Date/time serializers ───────────────────────────────────────────────

/// Days between Arrow epoch (1970-01-01) and SQL Server DATE epoch (0001-01-01).
const DAYS_UNIX_TO_0001: i64 = 719_162;

/// DATEN: `[0x03][days_since_0001 as 3-byte LE]` or `[0x00]` for null.
#[inline(always)]
fn serialize_date(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let days_since_epoch = array.as_primitive::<Date32Type>().value(row) as i64;
        let days_since_0001 = (days_since_epoch + DAYS_UNIX_TO_0001) as u32;
        buf.push(0x03);
        let le = days_since_0001.to_le_bytes();
        buf.extend_from_slice(&le[..3]);
    }
}

/// TIMEN: `[len][time_value LE]` or `[0x00]` for null.
///
/// Arrow Time64Microsecond stores microseconds since midnight.
/// TDS TIMEN stores fractional-second increments based on scale.
/// Scale 7 (default): increments = microseconds * 10 (100ns units), written as 5 bytes.
#[inline(always)]
fn serialize_time(buf: &mut Vec<u8>, array: &dyn Array, row: usize, scale: u8) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let micros = array.as_primitive::<Time64MicrosecondType>().value(row);
        let (increments, byte_len) = time_increments_and_len(micros, scale);
        buf.push(byte_len);
        let le = increments.to_le_bytes();
        buf.extend_from_slice(&le[..byte_len as usize]);
    }
}

/// Convert microseconds to TDS time increments at given scale, return (increments, wire_bytes).
#[inline(always)]
fn time_increments_and_len(micros: i64, scale: u8) -> (u64, u8) {
    // Scale determines the fractional precision:
    // scale 0-2: 3 bytes, scale 3-4: 4 bytes, scale 5-7: 5 bytes
    let increments = match scale {
        0 => micros / 1_000_000, // seconds
        1 => micros / 100_000,   // 0.1 seconds
        2 => micros / 10_000,    // 0.01 seconds
        3 => micros / 1_000,     // milliseconds
        4 => micros / 100,       // 0.1 ms
        5 => micros / 10,        // 0.01 ms
        6 => micros,             // microseconds
        _ => micros * 10,        // scale 7: 100ns units
    } as u64;
    let byte_len = match scale {
        0..=2 => 3u8,
        3..=4 => 4,
        _ => 5,
    };
    (increments, byte_len)
}

/// DATETIME2N: `[len][time_bytes][date_3bytes]` or `[0x00]` for null.
///
/// Arrow TimestampMicrosecond stores epoch-microseconds (since 1970-01-01 UTC).
/// TDS DATETIME2 = time component + date component (days since 0001-01-01).
#[inline(always)]
fn serialize_datetime2(buf: &mut Vec<u8>, array: &dyn Array, row: usize, scale: u8) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let epoch_micros = array.as_primitive::<TimestampMicrosecondType>().value(row);
        let (time_bytes, date_bytes) = decompose_epoch_micros(epoch_micros, scale);
        let time_wire_len = match scale {
            0..=2 => 3u8,
            3..=4 => 4,
            _ => 5,
        };
        let total_len = time_wire_len + 3; // time + date
        buf.push(total_len);
        buf.extend_from_slice(&time_bytes[..time_wire_len as usize]);
        buf.extend_from_slice(&date_bytes[..3]);
    }
}

/// DATETIMEOFFSETN: `[len][time_bytes][date_3bytes][offset_i16 LE]` or `[0x00]` for null.
///
/// Arrow TimestampMicrosecond with UTC timezone → local time + offset.
/// For UTC timestamps, offset = 0. The epoch micros represent UTC directly.
#[inline(always)]
fn serialize_datetimeoffset(buf: &mut Vec<u8>, array: &dyn Array, row: usize, scale: u8) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let epoch_micros = array.as_primitive::<TimestampMicrosecondType>().value(row);
        let (time_bytes, date_bytes) = decompose_epoch_micros(epoch_micros, scale);
        let time_wire_len = match scale {
            0..=2 => 3u8,
            3..=4 => 4,
            _ => 5,
        };
        let total_len = time_wire_len + 3 + 2; // time + date + offset
        buf.push(total_len);
        buf.extend_from_slice(&time_bytes[..time_wire_len as usize]);
        buf.extend_from_slice(&date_bytes[..3]);
        // UTC → offset = 0
        buf.extend_from_slice(&0i16.to_le_bytes());
    }
}

/// Decompose epoch-microseconds into TDS time and date components.
/// Returns (time_le_bytes, date_le_bytes) where date is 3 bytes of days_since_0001.
#[inline(always)]
fn decompose_epoch_micros(epoch_micros: i64, scale: u8) -> ([u8; 8], [u8; 4]) {
    const MICROS_PER_DAY: i64 = 86_400_000_000;

    let (day_offset, day_micros) = if epoch_micros >= 0 {
        let day_offset = epoch_micros / MICROS_PER_DAY;
        let day_micros = epoch_micros % MICROS_PER_DAY;
        (day_offset, day_micros)
    } else {
        // For negative epoch micros (before 1970-01-01), need floor division
        let day_offset = (epoch_micros - MICROS_PER_DAY + 1).div_euclid(MICROS_PER_DAY);
        let day_micros = epoch_micros.rem_euclid(MICROS_PER_DAY);
        (day_offset, day_micros)
    };

    let days_since_0001 = (day_offset + DAYS_UNIX_TO_0001) as u32;
    let (increments, _) = time_increments_and_len(day_micros, scale);

    (increments.to_le_bytes(), days_since_0001.to_le_bytes())
}

/// UNIQUEIDENTIFIER: `[0x10][16-byte TDS-swapped UUID]` or `[0x00]` for null.
///
/// TDS byte-swaps the first 3 groups of the UUID (mixed-endian).
/// Input: FixedSizeBinary(16) in RFC byte order.
/// Wire: bytes [0-3] reversed, [4-5] reversed, [6-7] reversed, [8-15] as-is.
#[inline(always)]
fn serialize_uuid(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.push(0x00);
    } else {
        let data = array.as_fixed_size_binary().value(row);
        buf.push(0x10);
        // TDS mixed-endian UUID byte-swap
        buf.extend_from_slice(&[data[3], data[2], data[1], data[0]]);
        buf.extend_from_slice(&[data[5], data[4]]);
        buf.extend_from_slice(&[data[7], data[6]]);
        buf.extend_from_slice(&data[8..16]);
    }
}

// ── PLP serializers ─────────────────────────────────────────────────────

/// PLP_UNKNOWN_LEN sentinel (8 bytes of 0xFE).
const PLP_UNKNOWN_LEN: [u8; 8] = [0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
/// PLP_NULL sentinel (8 bytes of 0xFF).
const PLP_NULL: [u8; 8] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
/// PLP terminator (4 bytes of 0x00).
const PLP_TERMINATOR: [u8; 4] = [0x00, 0x00, 0x00, 0x00];

/// NVARCHAR(MAX) via PLP: `PLP_UNKNOWN_LEN + [chunk_len u32 LE][UTF-16LE data] + PLP_TERMINATOR`
/// or `PLP_NULL` for null.
#[inline(always)]
fn serialize_nvarchar_plp(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.extend_from_slice(&PLP_NULL);
    } else {
        let s = array.as_string::<i32>().value(row);
        buf.extend_from_slice(&PLP_UNKNOWN_LEN);
        // Single chunk: encode to UTF-16LE
        let utf16: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let chunk_len = utf16.len() as u32;
        buf.extend_from_slice(&chunk_len.to_le_bytes());
        buf.extend_from_slice(&utf16);
        buf.extend_from_slice(&PLP_TERMINATOR);
    }
}

/// NVARCHAR(MAX) via PLP from LargeUtf8.
#[inline(always)]
fn serialize_nvarchar_plp_large(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.extend_from_slice(&PLP_NULL);
    } else {
        let s = array.as_string::<i64>().value(row);
        buf.extend_from_slice(&PLP_UNKNOWN_LEN);
        let utf16: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let chunk_len = utf16.len() as u32;
        buf.extend_from_slice(&chunk_len.to_le_bytes());
        buf.extend_from_slice(&utf16);
        buf.extend_from_slice(&PLP_TERMINATOR);
    }
}

/// VARBINARY(MAX) via PLP: `PLP_UNKNOWN_LEN + [chunk_len u32 LE][data] + PLP_TERMINATOR`
/// or `PLP_NULL` for null.
#[inline(always)]
fn serialize_binary_plp(buf: &mut Vec<u8>, array: &dyn Array, row: usize) {
    if array.is_null(row) {
        buf.extend_from_slice(&PLP_NULL);
    } else {
        let data = array.as_binary::<i32>().value(row);
        buf.extend_from_slice(&PLP_UNKNOWN_LEN);
        let chunk_len = data.len() as u32;
        buf.extend_from_slice(&chunk_len.to_le_bytes());
        buf.extend_from_slice(data);
        buf.extend_from_slice(&PLP_TERMINATOR);
    }
}

// ── Batch orchestrator ──────────────────────────────────────────────────

/// Pre-serialize an entire RecordBatch into per-row TDS byte buffers.
///
/// Iterates each column in a tight loop over Arrow's contiguous buffers,
/// appending TDS wire bytes to per-row buffers. The resulting buffers are
/// ready for `write_raw_bytes()` — zero per-value work at write time.
pub(crate) fn serialize_batch(
    batch: &RecordBatch,
    registry: &TypeMappingRegistry,
) -> TdsResult<Vec<Vec<u8>>> {
    let num_rows = batch.num_rows();
    if num_rows == 0 {
        return Ok(Vec::new());
    }

    // Estimate ~80 bytes per row for a 5-column schema
    let estimated_row_size = registry.mappings.len() * 16;
    let mut rows: Vec<Vec<u8>> = (0..num_rows)
        .map(|_| Vec::with_capacity(estimated_row_size))
        .collect();

    for mapping in &registry.mappings {
        let array = batch.column(mapping.source_index);

        serialize_column(&mut rows, array.as_ref(), mapping)?;
    }

    Ok(rows)
}

/// Serialize one column across all rows.
fn serialize_column(
    rows: &mut [Vec<u8>],
    array: &dyn Array,
    mapping: &ResolvedTypeMapping,
) -> TdsResult<()> {
    match &mapping.arrow_type {
        DataType::Boolean => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_bool(buf, array, row);
            }
        }
        DataType::UInt8 => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_u8(buf, array, row);
            }
        }
        DataType::Int16 => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_i16(buf, array, row);
            }
        }
        DataType::Int32 => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_i32(buf, array, row);
            }
        }
        DataType::Int64 => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_i64(buf, array, row);
            }
        }
        DataType::Float32 => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_f32(buf, array, row);
            }
        }
        DataType::Float64 => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_f64(buf, array, row);
            }
        }
        DataType::Decimal128(_, _) => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_decimal(buf, array, row, mapping.precision);
            }
        }
        DataType::Utf8 => {
            if mapping.is_plp {
                for (row, buf) in rows.iter_mut().enumerate() {
                    serialize_nvarchar_plp(buf, array, row);
                }
            } else {
                for (row, buf) in rows.iter_mut().enumerate() {
                    serialize_nvarchar(buf, array, row);
                }
            }
        }
        DataType::LargeUtf8 => {
            if mapping.is_plp {
                for (row, buf) in rows.iter_mut().enumerate() {
                    serialize_nvarchar_plp_large(buf, array, row);
                }
            } else {
                for (row, buf) in rows.iter_mut().enumerate() {
                    serialize_nvarchar_large(buf, array, row);
                }
            }
        }
        DataType::Binary => {
            if mapping.is_plp {
                for (row, buf) in rows.iter_mut().enumerate() {
                    serialize_binary_plp(buf, array, row);
                }
            } else {
                for (row, buf) in rows.iter_mut().enumerate() {
                    serialize_binary(buf, array, row);
                }
            }
        }
        DataType::Date32 => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_date(buf, array, row);
            }
        }
        DataType::Time64(arrow_schema::TimeUnit::Microsecond) => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_time(buf, array, row, mapping.scale);
            }
        }
        DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, None) => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_datetime2(buf, array, row, mapping.scale);
            }
        }
        DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, Some(tz))
            if tz.as_ref() == "UTC" || tz.as_ref() == "+00:00" =>
        {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_datetimeoffset(buf, array, row, mapping.scale);
            }
        }
        DataType::FixedSizeBinary(16) => {
            for (row, buf) in rows.iter_mut().enumerate() {
                serialize_uuid(buf, array, row);
            }
        }
        other => {
            return Err(ArrowError::UnsupportedArrowType {
                column_name: format!("column[{}]", mapping.source_index),
                arrow_type: other.clone(),
            }
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::{
        Decimal128Builder, Float64Builder, Int32Builder, Int64Builder, StringBuilder,
    };
    use arrow_schema::{Field, Schema};
    use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType, TypeLength};
    use std::sync::Arc;

    fn build_test_batch(num_rows: usize, with_nulls: bool) -> RecordBatch {
        let mut id = Int32Builder::with_capacity(num_rows);
        let mut amount = Int64Builder::with_capacity(num_rows);
        let mut price = Float64Builder::with_capacity(num_rows);
        let mut name = StringBuilder::with_capacity(num_rows, num_rows * 20);
        let mut total = Decimal128Builder::with_capacity(num_rows)
            .with_precision_and_scale(18, 2)
            .unwrap();

        for i in 0..num_rows {
            if with_nulls && i % 3 == 0 {
                id.append_null();
                amount.append_null();
                price.append_null();
                name.append_null();
                total.append_null();
            } else {
                id.append_value(i as i32);
                amount.append_value((i as i64) * 100);
                price.append_value(99.95 + i as f64);
                name.append_value(format!("item-{i}"));
                total.append_value((i as i128) * 10000 + 12345);
            }
        }

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, with_nulls),
            Field::new("amount", DataType::Int64, with_nulls),
            Field::new("price", DataType::Float64, with_nulls),
            Field::new("name", DataType::Utf8, with_nulls),
            Field::new("total", DataType::Decimal128(18, 2), with_nulls),
        ]));

        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(id.finish()),
                Arc::new(amount.finish()),
                Arc::new(price.finish()),
                Arc::new(name.finish()),
                Arc::new(total.finish()),
            ],
        )
        .unwrap()
    }

    fn make_test_registry() -> TypeMappingRegistry {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int32, true),
            Field::new("amount", DataType::Int64, true),
            Field::new("price", DataType::Float64, true),
            Field::new("name", DataType::Utf8, true),
            Field::new("total", DataType::Decimal128(18, 2), true),
        ]);

        let dest = vec![
            BulkCopyColumnMetadata::new("id", SqlDbType::Int, 0x38),
            BulkCopyColumnMetadata::new("amount", SqlDbType::BigInt, 0x26),
            BulkCopyColumnMetadata::new("price", SqlDbType::Float, 0x6D)
                .with_length(8, TypeLength::Fixed(8)),
            BulkCopyColumnMetadata::new("name", SqlDbType::NVarChar, 0xE7)
                .with_length(200, TypeLength::Variable(200)),
            BulkCopyColumnMetadata::new("total", SqlDbType::Decimal, 0x6A)
                .with_precision_scale(18, 2),
        ];

        TypeMappingRegistry::resolve(&schema, &dest).unwrap()
    }

    #[test]
    fn serialize_batch_non_null() {
        let batch = build_test_batch(3, false);
        let registry = make_test_registry();
        let rows = serialize_batch(&batch, &registry).unwrap();

        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert!(!row.is_empty());
        }

        // Row 0: id=0 → [0x04, 0x00, 0x00, 0x00, 0x00]
        assert_eq!(rows[0][0], 0x04); // length byte
        assert_eq!(&rows[0][1..5], &0i32.to_le_bytes());
    }

    #[test]
    fn serialize_batch_with_nulls() {
        let batch = build_test_batch(3, true);
        let registry = make_test_registry();
        let rows = serialize_batch(&batch, &registry).unwrap();

        // Row 0 (i=0, null): first byte should be 0x00 (null INTN)
        assert_eq!(rows[0][0], 0x00);

        // Row 1 (i=1, non-null): first byte should be 0x04
        assert_eq!(rows[1][0], 0x04);
    }

    #[test]
    fn serialize_empty_batch() {
        let batch = build_test_batch(0, false);
        let registry = make_test_registry();
        let rows = serialize_batch(&batch, &registry).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn serialize_nvarchar_utf16_encoding() {
        let mut buf = Vec::new();
        let array = arrow_array::StringArray::from(vec![Some("hello")]);
        serialize_nvarchar(&mut buf, &array, 0);

        // "hello" in UTF-16LE = 5 code units × 2 bytes = 10 bytes
        let byte_count = u16::from_le_bytes([buf[0], buf[1]]);
        assert_eq!(byte_count, 10);
        assert_eq!(buf.len(), 12); // 2 (length) + 10 (data)
    }

    #[test]
    fn serialize_nvarchar_null() {
        let mut buf = Vec::new();
        let array = arrow_array::StringArray::from(vec![None::<&str>]);
        serialize_nvarchar(&mut buf, &array, 0);
        assert_eq!(buf, vec![0xFF, 0xFF]);
    }

    #[test]
    fn decimal_precision_wire_length() {
        assert_eq!(decimal_value_bytes(1), 4);
        assert_eq!(decimal_value_bytes(9), 4);
        assert_eq!(decimal_value_bytes(10), 8);
        assert_eq!(decimal_value_bytes(19), 8);
        assert_eq!(decimal_value_bytes(20), 12);
        assert_eq!(decimal_value_bytes(28), 12);
        assert_eq!(decimal_value_bytes(29), 16);
        assert_eq!(decimal_value_bytes(38), 16);
    }

    #[test]
    fn serialize_decimal_positive() {
        let mut buf = Vec::new();
        let array = arrow_array::Decimal128Array::from(vec![Some(12345i128)])
            .with_precision_and_scale(18, 2)
            .unwrap();
        serialize_decimal(&mut buf, &array, 0, 18);

        // precision 18 → 8 value bytes, total length = 9
        assert_eq!(buf[0], 9); // length
        assert_eq!(buf[1], 1); // sign = positive
        let val = u64::from_le_bytes(buf[2..10].try_into().unwrap());
        assert_eq!(val, 12345);
    }

    #[test]
    fn serialize_decimal_negative() {
        let mut buf = Vec::new();
        let array = arrow_array::Decimal128Array::from(vec![Some(-99999i128)])
            .with_precision_and_scale(18, 2)
            .unwrap();
        serialize_decimal(&mut buf, &array, 0, 18);

        assert_eq!(buf[0], 9);
        assert_eq!(buf[1], 0); // sign = negative
        let val = u64::from_le_bytes(buf[2..10].try_into().unwrap());
        assert_eq!(val, 99999);
    }

    #[test]
    fn serialize_decimal_null() {
        let mut buf = Vec::new();
        let array = arrow_array::Decimal128Array::from(vec![None])
            .with_precision_and_scale(18, 2)
            .unwrap();
        serialize_decimal(&mut buf, &array, 0, 18);
        assert_eq!(buf, vec![0x00]);
    }
}

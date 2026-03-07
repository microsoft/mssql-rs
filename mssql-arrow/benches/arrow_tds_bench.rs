// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Benchmark: Arrow-direct vs Arrow-via-ColumnValues TDS bulk copy serialization.
//!
//! Measures the overhead of constructing intermediate `ColumnValues` enum variants
//! when serializing Arrow RecordBatch data into TDS ROW token wire format.

use arrow_array::builder::{
    Decimal128Builder, Float64Builder, Int32Builder, Int64Builder, StringBuilder,
};
use arrow_array::{
    Array, Decimal128Array, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::SqlString;
use std::hint::black_box;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Test data generation
// ---------------------------------------------------------------------------

/// Schema: id(INT), amount(BIGINT), price(FLOAT64), name(NVARCHAR), total(DECIMAL(18,2))
fn build_record_batch(num_rows: usize) -> RecordBatch {
    let mut id_builder = Int32Builder::with_capacity(num_rows);
    let mut amount_builder = Int64Builder::with_capacity(num_rows);
    let mut price_builder = Float64Builder::with_capacity(num_rows);
    let mut name_builder = StringBuilder::with_capacity(num_rows, num_rows * 20);
    let mut total_builder = Decimal128Builder::with_capacity(num_rows)
        .with_precision_and_scale(18, 2)
        .unwrap();

    for i in 0..num_rows {
        id_builder.append_value(i as i32);
        amount_builder.append_value((i as i64) * 100 + 42);
        price_builder.append_value(99.95 + i as f64 * 0.01);
        name_builder.append_value(format!("product-name-{i:06}"));
        // Decimal as i128 with scale 2: value 12345 → 123.45
        total_builder.append_value((i as i128) * 10000 + 12345);
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("amount", DataType::Int64, false),
        Field::new("price", DataType::Float64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("total", DataType::Decimal128(18, 2), false),
    ]));

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(id_builder.finish()),
            Arc::new(amount_builder.finish()),
            Arc::new(price_builder.finish()),
            Arc::new(name_builder.finish()),
            Arc::new(total_builder.finish()),
        ],
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// TDS wire-format serialization helpers (sync, into Vec<u8>)
//
// These produce the same byte layout as TdsValueSerializer for nullable
// bulk copy ROW tokens: [length_byte][value_bytes] for fixed-size types,
// [length_u16][utf16_bytes] for NVARCHAR.
// ---------------------------------------------------------------------------

/// INTN (0x26): [len=4][i32 LE]
#[inline(always)]
fn write_tds_int32(buf: &mut Vec<u8>, val: i32) {
    buf.push(4);
    buf.extend_from_slice(&val.to_le_bytes());
}

/// INTN (0x26): [len=8][i64 LE]
#[inline(always)]
fn write_tds_int64(buf: &mut Vec<u8>, val: i64) {
    buf.push(8);
    buf.extend_from_slice(&val.to_le_bytes());
}

/// FLOATN (0x6D): [len=8][f64 LE]
#[inline(always)]
fn write_tds_float64(buf: &mut Vec<u8>, val: f64) {
    buf.push(8);
    buf.extend_from_slice(&val.to_le_bytes());
}

/// NVARCHAR: [byte_count_u16 LE][UTF-16LE bytes]
#[inline(always)]
fn write_tds_nvarchar(buf: &mut Vec<u8>, val: &str) {
    let utf16: Vec<u16> = val.encode_utf16().collect();
    let byte_len = (utf16.len() * 2) as u16;
    buf.extend_from_slice(&byte_len.to_le_bytes());
    for unit in &utf16 {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
}

/// NVARCHAR from pre-encoded UTF-16LE bytes: [byte_count_u16 LE][raw bytes]
#[inline(always)]
fn write_tds_nvarchar_raw_utf16(buf: &mut Vec<u8>, utf16_bytes: &[u8]) {
    let byte_len = utf16_bytes.len() as u16;
    buf.extend_from_slice(&byte_len.to_le_bytes());
    buf.extend_from_slice(utf16_bytes);
}

/// DECIMALN (0x6A): [total_len][sign][value LE padded to precision-bucket]
/// For precision 1–9: 4 value bytes; 10–19: 8; 20–28: 12; 29–38: 16.
#[inline(always)]
fn write_tds_decimal128(buf: &mut Vec<u8>, val: i128, precision: u8) {
    let value_bytes = match precision {
        1..=9 => 4usize,
        10..=19 => 8,
        20..=28 => 12,
        _ => 16,
    };
    let total_len = 1 + value_bytes; // sign + value
    buf.push(total_len as u8);

    let (sign, abs) = if val >= 0 {
        (1u8, val as u128)
    } else {
        (0u8, (-val) as u128)
    };
    buf.push(sign);

    let bytes = abs.to_le_bytes();
    buf.extend_from_slice(&bytes[..value_bytes]);
}

// ---------------------------------------------------------------------------
// Path A: Arrow → TDS direct
//
// Reads typed values from Arrow arrays and writes TDS bytes directly.
// No intermediate Rust enum representation.
// ---------------------------------------------------------------------------

fn serialize_arrow_direct(batch: &RecordBatch, buf: &mut Vec<u8>) {
    let num_rows = batch.num_rows();
    let ids = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    let amounts = batch
        .column(1)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let prices = batch
        .column(2)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    let names = batch
        .column(3)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let totals = batch
        .column(4)
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .unwrap();

    for row in 0..num_rows {
        buf.push(0xD1); // ROW token
        write_tds_int32(buf, ids.value(row));
        write_tds_int64(buf, amounts.value(row));
        write_tds_float64(buf, prices.value(row));
        write_tds_nvarchar(buf, names.value(row));
        write_tds_decimal128(buf, totals.value(row), 18);
    }
}

// ---------------------------------------------------------------------------
// Path B: Arrow → ColumnValues → TDS
//
// First converts Arrow values into ColumnValues enum variants (the
// intermediate Rust representation), then serializes those to TDS bytes.
// This mirrors the DefaultRowWriter → TdsValueSerializer path.
// ---------------------------------------------------------------------------

/// Extract one row from Arrow RecordBatch into Vec<ColumnValues>.
#[inline(always)]
fn arrow_row_to_column_values(batch: &RecordBatch, row: usize) -> Vec<ColumnValues> {
    let ids = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    let amounts = batch
        .column(1)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let prices = batch
        .column(2)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    let names = batch
        .column(3)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let totals = batch
        .column(4)
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .unwrap();

    let name_str = names.value(row);
    let mut utf16_bytes = Vec::with_capacity(name_str.len() * 2);
    for unit in name_str.encode_utf16() {
        utf16_bytes.extend_from_slice(&unit.to_le_bytes());
    }

    let total_i128 = totals.value(row);
    let (is_positive, abs_val) = if total_i128 >= 0 {
        (true, total_i128 as u128)
    } else {
        (false, (-total_i128) as u128)
    };
    let mut int_parts = Vec::new();
    let mut remaining = abs_val;
    loop {
        int_parts.push((remaining & 0xFFFFFFFF) as i32);
        remaining >>= 32;
        if remaining == 0 {
            break;
        }
    }

    vec![
        ColumnValues::Int(ids.value(row)),
        ColumnValues::BigInt(amounts.value(row)),
        ColumnValues::Float(prices.value(row)),
        ColumnValues::String(SqlString::new(
            utf16_bytes,
            mssql_tds::datatypes::sql_string::EncodingType::Utf16,
        )),
        ColumnValues::Decimal(DecimalParts {
            is_positive,
            scale: 2,
            precision: 18,
            int_parts,
        }),
    ]
}

/// Serialize a Vec<ColumnValues> row into TDS bytes.
fn serialize_column_values_row(row: &[ColumnValues], buf: &mut Vec<u8>) {
    buf.push(0xD1); // ROW token
    for val in row {
        match val {
            ColumnValues::Int(v) => write_tds_int32(buf, *v),
            ColumnValues::BigInt(v) => write_tds_int64(buf, *v),
            ColumnValues::Float(v) => write_tds_float64(buf, *v),
            ColumnValues::String(s) => {
                if let Some(utf16) = s.as_utf16_bytes() {
                    write_tds_nvarchar_raw_utf16(buf, utf16);
                }
            }
            ColumnValues::Decimal(parts) => {
                let mut val_i128: i128 = 0;
                for (i, &part) in parts.int_parts.iter().enumerate() {
                    val_i128 |= (part as u32 as i128) << (i * 32);
                }
                if !parts.is_positive {
                    val_i128 = -val_i128;
                }
                write_tds_decimal128(buf, val_i128, parts.precision);
            }
            _ => {} // only the types we use
        }
    }
}

fn serialize_arrow_via_column_values(batch: &RecordBatch, buf: &mut Vec<u8>) {
    let num_rows = batch.num_rows();
    for row in 0..num_rows {
        let values = arrow_row_to_column_values(batch, row);
        serialize_column_values_row(&values, buf);
    }
}

// ---------------------------------------------------------------------------
// Path C: Arrow → TDS direct with pre-encoded UTF-16 (batch-level)
//
// Pre-encodes all strings to UTF-16 at the batch level before row iteration,
// demonstrating what a production zero-copy path could look like.
// ---------------------------------------------------------------------------

fn serialize_arrow_direct_preencoded(batch: &RecordBatch, buf: &mut Vec<u8>) {
    let num_rows = batch.num_rows();
    let ids = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    let amounts = batch
        .column(1)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let prices = batch
        .column(2)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    let names = batch
        .column(3)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let totals = batch
        .column(4)
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .unwrap();

    // Pre-encode all strings to UTF-16LE
    let encoded_names: Vec<Vec<u8>> = (0..num_rows)
        .map(|row| {
            let s = names.value(row);
            let mut bytes = Vec::with_capacity(s.len() * 2);
            for unit in s.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes
        })
        .collect();

    for (row, encoded_name) in encoded_names.iter().enumerate() {
        buf.push(0xD1);
        write_tds_int32(buf, ids.value(row));
        write_tds_int64(buf, amounts.value(row));
        write_tds_float64(buf, prices.value(row));
        write_tds_nvarchar_raw_utf16(buf, encoded_name);
        write_tds_decimal128(buf, totals.value(row), 18);
    }
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_arrow_tds_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("arrow_to_tds_bulk_copy");

    for &num_rows in &[10_000, 100_000] {
        let batch = build_record_batch(num_rows);
        // Estimate output size: ~50 bytes per row (1 + 5 + 9 + 9 + ~40 + 11)
        let est_size = num_rows * 80;

        group.throughput(Throughput::Elements(num_rows as u64));

        group.bench_with_input(
            BenchmarkId::new("arrow_direct", num_rows),
            &batch,
            |b, batch| {
                let mut buf = Vec::with_capacity(est_size);
                b.iter(|| {
                    buf.clear();
                    serialize_arrow_direct(black_box(batch), &mut buf);
                    black_box(buf.len());
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("arrow_direct_preencoded", num_rows),
            &batch,
            |b, batch| {
                let mut buf = Vec::with_capacity(est_size);
                b.iter(|| {
                    buf.clear();
                    serialize_arrow_direct_preencoded(black_box(batch), &mut buf);
                    black_box(buf.len());
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("arrow_via_column_values", num_rows),
            &batch,
            |b, batch| {
                let mut buf = Vec::with_capacity(est_size);
                b.iter(|| {
                    buf.clear();
                    serialize_arrow_via_column_values(black_box(batch), &mut buf);
                    black_box(buf.len());
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_arrow_tds_serialization);
criterion_main!(benches);

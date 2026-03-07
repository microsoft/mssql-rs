// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end benchmark: Arrow-direct vs Arrow-via-materialized-ColumnValues
//! bulk copy to a real SQL Server instance.
//!
//! Run with:
//!   DB_HOST=localhost DB_USERNAME=sa TRUST_SERVER_CERTIFICATE=true \
//!     cargo nextest run -p mssql-arrow --test bench_arrow_bulk_copy -- --ignored

mod common;

use arrow_array::builder::{
    Decimal128Builder, Float64Builder, Int32Builder, Int64Builder, StringBuilder,
};
use arrow_array::{
    Decimal128Array, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use async_trait::async_trait;
use common::{begin_connection, build_tcp_datasource};
use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;
use std::sync::Arc;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Arrow test data
// ---------------------------------------------------------------------------

fn build_record_batch(num_rows: usize) -> RecordBatch {
    let mut id = Int32Builder::with_capacity(num_rows);
    let mut amount = Int64Builder::with_capacity(num_rows);
    let mut price = Float64Builder::with_capacity(num_rows);
    let mut name = StringBuilder::with_capacity(num_rows, num_rows * 20);
    let mut total = Decimal128Builder::with_capacity(num_rows)
        .with_precision_and_scale(18, 2)
        .unwrap();

    for i in 0..num_rows {
        id.append_value(i as i32);
        amount.append_value((i as i64) * 100 + 42);
        price.append_value(99.95 + i as f64 * 0.01);
        name.append_value(format!("product-name-{i:06}"));
        total.append_value((i as i128) * 10000 + 12345);
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
            Arc::new(id.finish()),
            Arc::new(amount.finish()),
            Arc::new(price.finish()),
            Arc::new(name.finish()),
            Arc::new(total.finish()),
        ],
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Path A: Arrow-direct — pre-serialize TDS bytes from Arrow column buffers
//
// Walks each Arrow column's contiguous buffer in a tight loop, serializing
// TDS wire bytes for ALL rows of that column before moving to the next column.
// The per-row BulkLoadRow::write_to_packet then just writes the pre-built
// byte slice via write_raw_bytes — zero per-value work in the hot path.
// ---------------------------------------------------------------------------

/// TDS wire bytes for INTN (nullable i32): [len=4][i32 LE]
#[inline(always)]
fn tds_intn_i32(buf: &mut Vec<u8>, val: i32) {
    buf.push(4);
    buf.extend_from_slice(&val.to_le_bytes());
}

/// TDS wire bytes for INTN (nullable i64): [len=8][i64 LE]
#[inline(always)]
fn tds_intn_i64(buf: &mut Vec<u8>, val: i64) {
    buf.push(8);
    buf.extend_from_slice(&val.to_le_bytes());
}

/// TDS wire bytes for FLOATN (nullable f64): [len=8][f64 LE]
#[inline(always)]
fn tds_floatn_f64(buf: &mut Vec<u8>, val: f64) {
    buf.push(8);
    buf.extend_from_slice(&val.to_le_bytes());
}

/// TDS wire bytes for NVARCHAR: [byte_count_u16 LE][UTF-16LE data]
#[inline(always)]
fn tds_nvarchar(buf: &mut Vec<u8>, s: &str) {
    let start = buf.len();
    buf.extend_from_slice(&[0, 0]); // placeholder for byte count
    for code_unit in s.encode_utf16() {
        buf.extend_from_slice(&code_unit.to_le_bytes());
    }
    let byte_len = (buf.len() - start - 2) as u16;
    buf[start..start + 2].copy_from_slice(&byte_len.to_le_bytes());
}

/// TDS wire bytes for DECIMALN (precision 10–19): [len=9][sign][8-byte value LE]
#[inline(always)]
fn tds_decimal_p18(buf: &mut Vec<u8>, val: i128) {
    // Precision 18 → 8 value bytes. Total length = 1 (sign) + 8 (value) = 9.
    buf.push(9);
    let (sign, abs) = if val >= 0 {
        (1u8, val as u128)
    } else {
        (0u8, (-val) as u128)
    };
    buf.push(sign);
    buf.extend_from_slice(&(abs as u64).to_le_bytes());
}

/// Pre-serializes an Arrow RecordBatch into per-row TDS byte slices.
///
/// Processes each column in a tight loop over Arrow's contiguous buffers,
/// then assembles per-row byte slices via offset tracking.
fn pre_serialize_arrow_to_tds(batch: &RecordBatch) -> Vec<Vec<u8>> {
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

    // Estimate ~80 bytes per row: 5 + 9 + 9 + ~44 (20-char NVARCHAR) + 11
    let mut rows: Vec<Vec<u8>> = (0..num_rows).map(|_| Vec::with_capacity(80)).collect();

    // Column 0: INT — tight loop over i32 values buffer
    for (row, buf) in rows.iter_mut().enumerate() {
        tds_intn_i32(buf, ids.value(row));
    }

    // Column 1: BIGINT — tight loop over i64 values buffer
    for (row, buf) in rows.iter_mut().enumerate() {
        tds_intn_i64(buf, amounts.value(row));
    }

    // Column 2: FLOAT — tight loop over f64 values buffer
    for (row, buf) in rows.iter_mut().enumerate() {
        tds_floatn_f64(buf, prices.value(row));
    }

    // Column 3: NVARCHAR — UTF-8 → UTF-16LE transcoding per string
    for (row, buf) in rows.iter_mut().enumerate() {
        tds_nvarchar(buf, names.value(row));
    }

    // Column 4: DECIMAL(18,2) — tight loop over i128 values buffer
    for (row, buf) in rows.iter_mut().enumerate() {
        tds_decimal_p18(buf, totals.value(row));
    }

    rows
}

/// A row whose TDS column bytes have been pre-serialized.
/// write_to_packet just writes the blob — zero per-value work.
struct PreSerializedRow {
    tds_bytes: Vec<u8>,
    col_count: usize,
}

#[async_trait]
impl BulkLoadRow for PreSerializedRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer.write_raw_bytes(&self.tds_bytes).await?;
        *column_index += self.col_count;
        Ok(())
    }
}

#[async_trait]
impl BulkLoadRow for &PreSerializedRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer.write_raw_bytes(&self.tds_bytes).await?;
        *column_index += self.col_count;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Path B: Arrow → materialized Vec<ColumnValues> → BulkLoadRow
//
// Pre-converts entire RecordBatch into row-major Vec<Vec<ColumnValues>>,
// simulating the intermediate representation path. The BulkLoadRow impl
// then writes from the pre-materialized data.
// ---------------------------------------------------------------------------

struct MaterializedRow {
    values: Vec<ColumnValues>,
}

#[async_trait]
impl BulkLoadRow for MaterializedRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        for val in &self.values {
            writer.write_column_value(*column_index, val).await?;
            *column_index += 1;
        }
        Ok(())
    }
}

#[async_trait]
impl BulkLoadRow for &MaterializedRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        for val in &self.values {
            writer.write_column_value(*column_index, val).await?;
            *column_index += 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Path C: Arrow-streaming — write TDS bytes directly to the packet buffer
//
// Like Path A, we walk Arrow column buffers. But instead of accumulating into
// a per-row Vec<u8>, we write each column's TDS bytes directly into the
// PacketWriter's internal buffer via the unchecked write methods. No
// intermediate allocation at all.
// ---------------------------------------------------------------------------

/// Write INTN i32 TDS bytes directly to the streaming writer.
#[inline(always)]
fn stream_intn_i32(writer: &mut StreamingBulkLoadWriter<'_>, val: i32) {
    writer.write_byte_unchecked(4);
    writer.write_i32_unchecked(val);
}

/// Write INTN i64 TDS bytes directly to the streaming writer.
#[inline(always)]
fn stream_intn_i64(writer: &mut StreamingBulkLoadWriter<'_>, val: i64) {
    writer.write_byte_unchecked(8);
    writer.write_i64_unchecked(val);
}

/// Write FLOATN f64 TDS bytes directly to the streaming writer.
#[inline(always)]
fn stream_floatn_f64(writer: &mut StreamingBulkLoadWriter<'_>, val: f64) {
    writer.write_byte_unchecked(8);
    writer.write_f64_unchecked(val);
}

/// Write NVARCHAR TDS bytes directly to the streaming writer.
/// Zero-alloc: writes placeholder u16, transcodes UTF-16 inline, patches length.
#[inline(always)]
fn stream_nvarchar(writer: &mut StreamingBulkLoadWriter<'_>, s: &str) {
    let len_pos = writer.unchecked_position();
    writer.write_u16_unchecked(0); // placeholder
    let mut byte_len: u16 = 0;
    for code_unit in s.encode_utf16() {
        writer.write_bytes_unchecked(&code_unit.to_le_bytes());
        byte_len += 2;
    }
    writer.write_u16_at_position(len_pos, byte_len);
}

/// Write DECIMALN(18,2) TDS bytes directly to the streaming writer.
#[inline(always)]
fn stream_decimal_p18(writer: &mut StreamingBulkLoadWriter<'_>, val: i128) {
    writer.write_byte_unchecked(9); // len = 1 (sign) + 8 (value)
    let (sign, abs) = if val >= 0 {
        (1u8, val as u128)
    } else {
        (0u8, (-val) as u128)
    };
    writer.write_byte_unchecked(sign);
    writer.write_i64_unchecked(abs as i64);
}

/// A streaming row that writes Arrow values directly to the packet buffer.
/// Holds references to the Arrow arrays and the row index.
struct StreamingRow<'a> {
    row: usize,
    ids: &'a Int32Array,
    amounts: &'a Int64Array,
    prices: &'a Float64Array,
    names: &'a StringArray,
    totals: &'a Decimal128Array,
}

#[async_trait]
impl BulkLoadRow for StreamingRow<'_> {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        // Each row is at most ~80 bytes (5+9+9+~44+11). TDS packet is ~4KB.
        // If space is tight, flush first; otherwise write unchecked.
        if !writer.has_space(256) {
            writer.flush_if_needed().await?;
        }
        stream_intn_i32(writer, self.ids.value(self.row));
        stream_intn_i64(writer, self.amounts.value(self.row));
        stream_floatn_f64(writer, self.prices.value(self.row));
        stream_nvarchar(writer, self.names.value(self.row));
        stream_decimal_p18(writer, self.totals.value(self.row));
        writer.flush_if_needed().await?;
        *column_index += 5;
        Ok(())
    }
}

#[async_trait]
impl BulkLoadRow for &StreamingRow<'_> {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        if !writer.has_space(256) {
            writer.flush_if_needed().await?;
        }
        stream_intn_i32(writer, self.ids.value(self.row));
        stream_intn_i64(writer, self.amounts.value(self.row));
        stream_floatn_f64(writer, self.prices.value(self.row));
        stream_nvarchar(writer, self.names.value(self.row));
        stream_decimal_p18(writer, self.totals.value(self.row));
        writer.flush_if_needed().await?;
        *column_index += 5;
        Ok(())
    }
}

fn materialize_batch(batch: &RecordBatch) -> Vec<MaterializedRow> {
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

    (0..num_rows)
        .map(|row| {
            let s = names.value(row);
            let mut utf16_bytes = Vec::with_capacity(s.len() * 2);
            for unit in s.encode_utf16() {
                utf16_bytes.extend_from_slice(&unit.to_le_bytes());
            }

            let raw = totals.value(row);
            let parts = i128_to_decimal_parts(raw, 18, 2);

            MaterializedRow {
                values: vec![
                    ColumnValues::Int(ids.value(row)),
                    ColumnValues::BigInt(amounts.value(row)),
                    ColumnValues::Float(prices.value(row)),
                    ColumnValues::String(SqlString::new(utf16_bytes, EncodingType::Utf16)),
                    ColumnValues::Decimal(parts),
                ],
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn i128_to_decimal_parts(val: i128, precision: u8, scale: u8) -> DecimalParts {
    let (is_positive, abs_val) = if val >= 0 {
        (true, val as u128)
    } else {
        (false, (-val) as u128)
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
    DecimalParts {
        is_positive,
        scale,
        precision,
        int_parts,
    }
}

const CREATE_TABLE: &str = "
    IF OBJECT_ID('tempdb..#ArrowBenchDirect') IS NOT NULL DROP TABLE #ArrowBenchDirect;
    IF OBJECT_ID('tempdb..#ArrowBenchMaterialized') IS NOT NULL DROP TABLE #ArrowBenchMaterialized;
    IF OBJECT_ID('tempdb..#ArrowBenchStreaming') IS NOT NULL DROP TABLE #ArrowBenchStreaming;
    IF OBJECT_ID('tempdb..#ArrowBenchArrowBulkCopy') IS NOT NULL DROP TABLE #ArrowBenchArrowBulkCopy;
    CREATE TABLE #ArrowBenchDirect (
        id INT NOT NULL,
        amount BIGINT NOT NULL,
        price FLOAT NOT NULL,
        name NVARCHAR(200) NOT NULL,
        total DECIMAL(18,2) NOT NULL
    );
    CREATE TABLE #ArrowBenchMaterialized (
        id INT NOT NULL,
        amount BIGINT NOT NULL,
        price FLOAT NOT NULL,
        name NVARCHAR(200) NOT NULL,
        total DECIMAL(18,2) NOT NULL
    );
    CREATE TABLE #ArrowBenchStreaming (
        id INT NOT NULL,
        amount BIGINT NOT NULL,
        price FLOAT NOT NULL,
        name NVARCHAR(200) NOT NULL,
        total DECIMAL(18,2) NOT NULL
    );
    CREATE TABLE #ArrowBenchArrowBulkCopy (
        id INT NOT NULL,
        amount BIGINT NOT NULL,
        price FLOAT NOT NULL,
        name NVARCHAR(200) NOT NULL,
        total DECIMAL(18,2) NOT NULL
    );
";

const TRUNCATE_DIRECT: &str = "TRUNCATE TABLE #ArrowBenchDirect";
const TRUNCATE_MATERIALIZED: &str = "TRUNCATE TABLE #ArrowBenchMaterialized";
const TRUNCATE_STREAMING: &str = "TRUNCATE TABLE #ArrowBenchStreaming";
const TRUNCATE_ARROW_BULK_COPY: &str = "TRUNCATE TABLE #ArrowBenchArrowBulkCopy";

const NUM_ROWS: usize = 100_000;
const NUM_ITERATIONS: usize = 10;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // Requires a live SQL Server — run explicitly
async fn bench_arrow_bulk_copy_to_sql_server() {
    let datasource = build_tcp_datasource();
    let mut client = begin_connection(&datasource).await;

    // Create tables
    client
        .execute(CREATE_TABLE.to_string(), None, None)
        .await
        .expect("Failed to create test tables");
    client.close_query().await.expect("close_query failed");

    let batch = build_record_batch(NUM_ROWS);

    println!("\n============================================================");
    println!("Arrow Bulk Copy Benchmark — {NUM_ROWS} rows × {NUM_ITERATIONS} iterations");
    println!("Schema: INT, BIGINT, FLOAT, NVARCHAR(200), DECIMAL(18,2)");
    println!("Paths: A=pre-serialized, B=materialized, C=streaming, D=ArrowBulkCopy");
    println!("============================================================\n");

    // ── Path A: Arrow-direct (pre-serialized TDS bytes) ────────────────
    let mut direct_times = Vec::with_capacity(NUM_ITERATIONS);

    for iter in 0..NUM_ITERATIONS {
        // Truncate between iterations
        if iter > 0 {
            client
                .execute(TRUNCATE_DIRECT.to_string(), None, None)
                .await
                .unwrap();
            client.close_query().await.unwrap();
        }

        // Pre-serialize Arrow batch → per-row TDS byte buffers (this IS the direct path)
        let start = Instant::now();
        let tds_rows = pre_serialize_arrow_to_tds(&batch);
        let rows: Vec<PreSerializedRow> = tds_rows
            .into_iter()
            .map(|tds_bytes| PreSerializedRow {
                tds_bytes,
                col_count: 5,
            })
            .collect();
        {
            let bulk_copy = BulkCopy::new(&mut client, "#ArrowBenchDirect");
            bulk_copy
                .batch_size(NUM_ROWS)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Arrow-direct bulk copy failed");
        }
        let elapsed = start.elapsed();
        direct_times.push(elapsed);
        println!(
            "  [direct]       iter {}: {:>8.2} ms  ({:.1} Krows/s)",
            iter + 1,
            elapsed.as_secs_f64() * 1000.0,
            NUM_ROWS as f64 / elapsed.as_secs_f64() / 1000.0,
        );
    }

    // ── Path B: Arrow → materialized ColumnValues ───────────────────────
    let mut materialized_times = Vec::with_capacity(NUM_ITERATIONS);

    for iter in 0..NUM_ITERATIONS {
        if iter > 0 {
            client
                .execute(TRUNCATE_MATERIALIZED.to_string(), None, None)
                .await
                .unwrap();
            client.close_query().await.unwrap();
        }

        // Materialize Arrow → Vec<MaterializedRow> (this is part of the cost)
        let start = Instant::now();
        let rows = materialize_batch(&batch);
        {
            let bulk_copy = BulkCopy::new(&mut client, "#ArrowBenchMaterialized");
            bulk_copy
                .batch_size(NUM_ROWS)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Materialized bulk copy failed");
        }
        let elapsed = start.elapsed();
        materialized_times.push(elapsed);
        println!(
            "  [materialized] iter {}: {:>8.2} ms  ({:.1} Krows/s)",
            iter + 1,
            elapsed.as_secs_f64() * 1000.0,
            NUM_ROWS as f64 / elapsed.as_secs_f64() / 1000.0,
        );
    }

    // ── Path C: Arrow-streaming — write directly to packet buffer ──────
    let mut streaming_times = Vec::with_capacity(NUM_ITERATIONS);
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

    for iter in 0..NUM_ITERATIONS {
        if iter > 0 {
            client
                .execute(TRUNCATE_STREAMING.to_string(), None, None)
                .await
                .unwrap();
            client.close_query().await.unwrap();
        }

        let start = Instant::now();
        {
            let streaming_rows: Vec<StreamingRow<'_>> = (0..NUM_ROWS)
                .map(|row| StreamingRow {
                    row,
                    ids,
                    amounts,
                    prices,
                    names,
                    totals,
                })
                .collect();
            let bulk_copy = BulkCopy::new(&mut client, "#ArrowBenchStreaming");
            bulk_copy
                .batch_size(NUM_ROWS)
                .write_to_server_zerocopy(&streaming_rows)
                .await
                .expect("Streaming bulk copy failed");
        }
        let elapsed = start.elapsed();
        streaming_times.push(elapsed);
        println!(
            "  [streaming]    iter {}: {:>8.2} ms  ({:.1} Krows/s)",
            iter + 1,
            elapsed.as_secs_f64() * 1000.0,
            NUM_ROWS as f64 / elapsed.as_secs_f64() / 1000.0,
        );
    }

    // ── Path D: ArrowBulkCopy — high-level API ─────────────────────────
    let mut arrow_bulk_copy_times = Vec::with_capacity(NUM_ITERATIONS);

    for iter in 0..NUM_ITERATIONS {
        if iter > 0 {
            client
                .execute(TRUNCATE_ARROW_BULK_COPY.to_string(), None, None)
                .await
                .unwrap();
            client.close_query().await.unwrap();
        }

        let start = Instant::now();
        {
            let mut arrow_bc =
                mssql_arrow::ArrowBulkCopy::new(&mut client, "#ArrowBenchArrowBulkCopy");
            arrow_bc = arrow_bc.batch_size(NUM_ROWS);
            arrow_bc
                .write_batch(&batch)
                .await
                .expect("ArrowBulkCopy bulk copy failed");
        }
        let elapsed = start.elapsed();
        arrow_bulk_copy_times.push(elapsed);
        println!(
            "  [arrow_bulk]   iter {}: {:>8.2} ms  ({:.1} Krows/s)",
            iter + 1,
            elapsed.as_secs_f64() * 1000.0,
            NUM_ROWS as f64 / elapsed.as_secs_f64() / 1000.0,
        );
    }

    // ── Verify row counts ───────────────────────────────────────────────
    client
        .execute(
            "SELECT COUNT(*) FROM #ArrowBenchDirect".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
    let count = common::get_scalar_value(&mut client).await.unwrap();
    assert_eq!(count, Some(ColumnValues::Int(NUM_ROWS as i32)));

    client
        .execute(
            "SELECT COUNT(*) FROM #ArrowBenchMaterialized".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
    let count = common::get_scalar_value(&mut client).await.unwrap();
    assert_eq!(count, Some(ColumnValues::Int(NUM_ROWS as i32)));

    client
        .execute(
            "SELECT COUNT(*) FROM #ArrowBenchStreaming".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
    let count = common::get_scalar_value(&mut client).await.unwrap();
    assert_eq!(count, Some(ColumnValues::Int(NUM_ROWS as i32)));

    client
        .execute(
            "SELECT COUNT(*) FROM #ArrowBenchArrowBulkCopy".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
    let count = common::get_scalar_value(&mut client).await.unwrap();
    assert_eq!(count, Some(ColumnValues::Int(NUM_ROWS as i32)));

    // ── Summary ─────────────────────────────────────────────────────────
    let avg_direct =
        direct_times.iter().map(|d| d.as_secs_f64()).sum::<f64>() / NUM_ITERATIONS as f64;
    let avg_materialized = materialized_times
        .iter()
        .map(|d| d.as_secs_f64())
        .sum::<f64>()
        / NUM_ITERATIONS as f64;
    let avg_streaming =
        streaming_times.iter().map(|d| d.as_secs_f64()).sum::<f64>() / NUM_ITERATIONS as f64;
    let avg_arrow_bulk_copy = arrow_bulk_copy_times
        .iter()
        .map(|d| d.as_secs_f64())
        .sum::<f64>()
        / NUM_ITERATIONS as f64;

    // Skip first iteration (cold) for warmed-up averages
    let warm_direct = direct_times[1..]
        .iter()
        .map(|d| d.as_secs_f64())
        .sum::<f64>()
        / (NUM_ITERATIONS - 1) as f64;
    let warm_materialized = materialized_times[1..]
        .iter()
        .map(|d| d.as_secs_f64())
        .sum::<f64>()
        / (NUM_ITERATIONS - 1) as f64;
    let warm_streaming = streaming_times[1..]
        .iter()
        .map(|d| d.as_secs_f64())
        .sum::<f64>()
        / (NUM_ITERATIONS - 1) as f64;
    let warm_arrow_bulk_copy = arrow_bulk_copy_times[1..]
        .iter()
        .map(|d| d.as_secs_f64())
        .sum::<f64>()
        / (NUM_ITERATIONS - 1) as f64;

    let speedup_direct = avg_materialized / avg_direct;
    let speedup_streaming = avg_materialized / avg_streaming;
    let speedup_arrow_bulk_copy = avg_materialized / avg_arrow_bulk_copy;
    let warm_speedup_direct = warm_materialized / warm_direct;
    let warm_speedup_streaming = warm_materialized / warm_streaming;
    let warm_speedup_arrow_bulk_copy = warm_materialized / warm_arrow_bulk_copy;

    println!("\n------------------------------------------------------------");
    println!("RESULTS ({NUM_ROWS} rows, {NUM_ITERATIONS} iterations)");
    println!("------------------------------------------------------------");
    println!(
        "  Pre-serialized (avg):     {:>8.2} ms  ({:.1} Krows/s)",
        avg_direct * 1000.0,
        NUM_ROWS as f64 / avg_direct / 1000.0,
    );
    println!(
        "  Streaming (avg):          {:>8.2} ms  ({:.1} Krows/s)",
        avg_streaming * 1000.0,
        NUM_ROWS as f64 / avg_streaming / 1000.0,
    );
    println!(
        "  Materialized (avg):       {:>8.2} ms  ({:.1} Krows/s)",
        avg_materialized * 1000.0,
        NUM_ROWS as f64 / avg_materialized / 1000.0,
    );
    println!(
        "  ArrowBulkCopy (avg):      {:>8.2} ms  ({:.1} Krows/s)",
        avg_arrow_bulk_copy * 1000.0,
        NUM_ROWS as f64 / avg_arrow_bulk_copy / 1000.0,
    );
    println!("  Speedup pre-serial (all): {speedup_direct:.2}×");
    println!("  Speedup streaming (all):  {speedup_streaming:.2}×");
    println!("  Speedup arrow_bulk (all): {speedup_arrow_bulk_copy:.2}×");
    println!(
        "  Pre-serialized (warm):    {:>8.2} ms  ({:.1} Krows/s)",
        warm_direct * 1000.0,
        NUM_ROWS as f64 / warm_direct / 1000.0,
    );
    println!(
        "  Streaming (warm):         {:>8.2} ms  ({:.1} Krows/s)",
        warm_streaming * 1000.0,
        NUM_ROWS as f64 / warm_streaming / 1000.0,
    );
    println!(
        "  Materialized (warm):      {:>8.2} ms  ({:.1} Krows/s)",
        warm_materialized * 1000.0,
        NUM_ROWS as f64 / warm_materialized / 1000.0,
    );
    println!(
        "  ArrowBulkCopy (warm):     {:>8.2} ms  ({:.1} Krows/s)",
        warm_arrow_bulk_copy * 1000.0,
        NUM_ROWS as f64 / warm_arrow_bulk_copy / 1000.0,
    );
    println!("  Speedup pre-serial (warm):{warm_speedup_direct:.2}×");
    println!("  Speedup streaming (warm): {warm_speedup_streaming:.2}×");
    println!("  Speedup arrow_bulk (warm):{warm_speedup_arrow_bulk_copy:.2}×");
    println!("------------------------------------------------------------\n");
}

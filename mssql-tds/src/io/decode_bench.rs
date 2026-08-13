// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! In-crate row-decode benchmark.
//!
//! Drives [`receive_row_into_internal`] over a pre-built buffer so the measurement
//! covers packet reading, the token switch and column decode without a server,
//! a socket, or Criterion's sampling in the loop.
//!
//! Ignored by default. Run in release, or the numbers describe `debug` codegen:
//!
//! ```text
//! cargo nextest run -p mssql-tds --lib --release --run-ignored all -E 'test(decode_bench)' --no-capture
//! ```
//!
//! `BENCH_ROWS` (default `20000`) sets rows per measured pass.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::TdsResult;
use crate::datatypes::row_writer::{DefaultRowWriter, DiscardRowWriter, RowWriter};
use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfo};
use crate::io::packet_reader::TdsPacketReader;
use crate::io::token_stream::{
    ColumnPolicy, GenericTokenParserRegistry, ParserContext, receive_row_into_internal,
};
use crate::query::metadata::ColumnMetadata;
use crate::token::tokens::{ColMetadataToken, SqlCollation, TokenType};

const WARMUP_PASSES: usize = 3;
const MEASURED_PASSES: usize = 9;
const DEFAULT_ROWS: usize = 20_000;

fn rows_per_pass() -> usize {
    std::env::var("BENCH_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ROWS)
}

/// Cursor over a pre-built buffer. Deliberately trivial so the benchmark measures
/// decode rather than the reader.
struct BenchReader {
    data: Arc<Vec<u8>>,
    pos: usize,
}

impl BenchReader {
    fn new(data: Arc<Vec<u8>>) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> TdsResult<&[u8]> {
        if self.pos + n > self.data.len() {
            return Err(crate::error::Error::ProtocolError(
                "unexpected end of bench buffer".to_string(),
            ));
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }
}

impl TdsPacketReader for BenchReader {
    async fn read_byte(&mut self) -> TdsResult<u8> {
        Ok(self.take(1)?[0])
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        let mut buf = [0u8; 8];
        buf[..5].copy_from_slice(self.take(5)?);
        Ok(u64::from_le_bytes(buf))
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        let n = buffer.len();
        buffer.copy_from_slice(self.take(n)?);
        Ok(n)
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        let mut buf = [0u8; 4];
        buf[..3].copy_from_slice(self.take(3)?);
        Ok(u32::from_le_bytes(buf))
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let len = self.read_byte().await? as usize;
        Ok(self.take(len)?.to_vec())
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let len = self.read_uint16().await? as usize;
        Ok(self.take(len)?.to_vec())
    }

    async fn skip_bytes(&mut self, count: usize) -> TdsResult<()> {
        self.take(count)?;
        Ok(())
    }

    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
        self.read_unicode_with_byte_length(string_length * 2).await
    }

    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String> {
        let bytes = self.take(byte_length)?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units)
            .map_err(|e| crate::error::Error::ProtocolError(format!("utf16: {e}")))
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        let len = self.read_byte().await? as usize;
        self.read_unicode(len).await
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        let len = self.read_uint16().await? as usize;
        Ok(Some(self.read_unicode(len).await?))
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        Ok(())
    }

    fn reset_reader(&mut self) {
        self.pos = 0;
    }
}

fn collation() -> SqlCollation {
    SqlCollation {
        info: 0x0409,
        lcid_language_id: 0x0409,
        col_flags: 0,
        sort_id: 52,
    }
}

fn int4_column(name: &str) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0,
        data_type: TdsDataType::Int4,
        type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

fn varchar_column(name: &str, len: usize) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0,
        data_type: TdsDataType::BigVarChar,
        type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, len, Some(collation()))
            .unwrap(),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

fn scaled_column(name: &str, data_type: TdsDataType, scale: u8, len: usize) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0,
        data_type,
        type_info: TypeInfo::var_len_scale(data_type, len, scale).unwrap(),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

fn decimal_column(name: &str, precision: u8, scale: u8) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0,
        data_type: TdsDataType::DecimalN,
        type_info: TypeInfo::var_len_precision_scale(TdsDataType::DecimalN, 17, precision, scale)
            .unwrap(),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

fn guid_column(name: &str) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0,
        data_type: TdsDataType::Guid,
        type_info: TypeInfo::var_len(TdsDataType::Guid, 16).unwrap(),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

/// The #238 benchmark schema: 39 `INT` + 9 `VARCHAR(6)`.
///
/// Narrow by construction — it exercises roughly two of the ten decode shapes,
/// which is exactly why a second schema exists below.
fn schema_int_varchar() -> Vec<ColumnMetadata> {
    let mut columns: Vec<ColumnMetadata> = (0..39).map(|i| int4_column(&format!("i{i}"))).collect();
    columns.extend((0..9).map(|i| varchar_column(&format!("s{i}"), 6)));
    columns
}

/// Shape-diverse schema: every hoistable classification appears at least once.
///
/// This is where a precomputed plan should pay, if it pays anywhere: `VARCHAR`
/// re-derives an encoding per cell, `DECIMAL` a precision/scale, and the
/// date/time types a scale.
fn schema_mixed() -> Vec<ColumnMetadata> {
    let mut columns = Vec::new();
    for i in 0..8 {
        columns.push(int4_column(&format!("i{i}")));
        columns.push(varchar_column(&format!("s{i}"), 16));
        columns.push(decimal_column(&format!("d{i}"), 18, 4));
        columns.push(scaled_column(&format!("t{i}"), TdsDataType::TimeN, 7, 5));
        columns.push(scaled_column(
            &format!("dt{i}"),
            TdsDataType::DateTime2N,
            7,
            8,
        ));
        columns.push(guid_column(&format!("g{i}")));
    }
    columns
}

/// Appends one cell's wire bytes for `column`.
fn push_cell(buffer: &mut Vec<u8>, column: &ColumnMetadata, row: usize) {
    match column.data_type {
        TdsDataType::Int4 => buffer.extend_from_slice(&(row as i32).to_le_bytes()),
        TdsDataType::BigVarChar => {
            let text = format!("r{row}");
            let bytes = text.as_bytes();
            buffer.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            buffer.extend_from_slice(bytes);
        }
        TdsDataType::DecimalN => {
            // 1 length byte + sign + 4-byte magnitude.
            buffer.push(5);
            buffer.push(1);
            buffer.extend_from_slice(&(row as u32).to_le_bytes());
        }
        TdsDataType::TimeN => {
            // scale 7 -> 5-byte time payload.
            buffer.push(5);
            buffer.extend_from_slice(&[0x00, 0x01, 0x02, 0x03, 0x04]);
        }
        TdsDataType::DateTime2N => {
            // scale 7 -> 5-byte time + 3-byte date.
            buffer.push(8);
            buffer.extend_from_slice(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x10, 0x20, 0x30]);
        }
        TdsDataType::Guid => {
            buffer.push(16);
            buffer.extend_from_slice(&[0xAB; 16]);
        }
        other => unreachable!("bench schema does not use {other:?}"),
    }
}

fn build_row_buffer(columns: &[ColumnMetadata], rows: usize) -> Arc<Vec<u8>> {
    let mut buffer = Vec::new();
    for row in 0..rows {
        buffer.push(TokenType::Row as u8);
        for column in columns {
            push_cell(&mut buffer, column, row);
        }
    }
    Arc::new(buffer)
}

/// Builds the metadata token, and with it the decode plan.
///
/// Isolated because this is the one call whose shape differs across the branches
/// this benchmark is used to compare.
fn metadata_token(columns: Vec<ColumnMetadata>) -> Arc<ColMetadataToken> {
    Arc::new(ColMetadataToken {
        column_count: columns.len() as u16,
        columns,
        cek_table: Vec::new(),
    })
}

/// Decodes every row in `buffer`, returning the elapsed time for the pass.
#[derive(Clone, Copy)]
enum Sink {
    /// Cheapest possible consumer: maximises decode's share of the total.
    Discard,
    /// Materialises `ColumnValues` per cell and hands the row off, which is how
    /// the public row API and the JS binding actually consume rows.
    Materialize,
}

impl Sink {
    fn label(self) -> &'static str {
        match self {
            Sink::Discard => "discard",
            Sink::Materialize => "materialize",
        }
    }
}

async fn drive<W: RowWriter + Send + ?Sized>(
    buffer: Arc<Vec<u8>>,
    context: &ParserContext,
    registry: &GenericTokenParserRegistry,
    rows: usize,
    writer: &mut W,
    mut after_row: impl FnMut(&mut W),
) -> Duration {
    let mut reader = BenchReader::new(buffer);

    let start = Instant::now();
    for _ in 0..rows {
        let result = receive_row_into_internal(
            &mut reader,
            registry,
            context,
            ColumnPolicy::DecodeAll,
            writer,
        )
        .await
        .expect("bench row must decode");
        std::hint::black_box(&result);
        after_row(writer);
    }
    start.elapsed()
}

async fn one_pass(
    buffer: Arc<Vec<u8>>,
    context: &ParserContext,
    registry: &GenericTokenParserRegistry,
    rows: usize,
    columns: usize,
    sink: Sink,
) -> Duration {
    match sink {
        Sink::Discard => {
            drive(
                buffer,
                context,
                registry,
                rows,
                &mut DiscardRowWriter,
                |_| {},
            )
            .await
        }
        Sink::Materialize => {
            let mut writer = DefaultRowWriter::new(columns);
            drive(buffer, context, registry, rows, &mut writer, |w| {
                std::hint::black_box(w.take_row());
            })
            .await
        }
    }
}

async fn measure(label: &str, columns: Vec<ColumnMetadata>, sink: Sink) {
    let rows = rows_per_pass();
    let column_count = columns.len();
    let buffer = build_row_buffer(&columns, rows);
    let bytes = buffer.len();
    let metadata = metadata_token(columns);
    let context = ParserContext::ColumnMetadata(Arc::clone(&metadata), None);
    let registry = GenericTokenParserRegistry::default();

    for _ in 0..WARMUP_PASSES {
        one_pass(
            Arc::clone(&buffer),
            &context,
            &registry,
            rows,
            column_count,
            sink,
        )
        .await;
    }

    let mut timings: Vec<Duration> = Vec::with_capacity(MEASURED_PASSES);
    for _ in 0..MEASURED_PASSES {
        timings.push(
            one_pass(
                Arc::clone(&buffer),
                &context,
                &registry,
                rows,
                column_count,
                sink,
            )
            .await,
        );
    }
    timings.sort();

    let median = timings[MEASURED_PASSES / 2];
    let cells = rows * column_count;
    let ns_per_cell = median.as_nanos() as f64 / cells as f64;
    let sink = sink.label();

    println!(
        "{label:<18} sink={sink:<12} cols={column_count:<3} rows={rows} cells={cells} \
         bytes={bytes} median={median:?} min={:?} max={:?} ns/cell={ns_per_cell:.2}",
        timings[0],
        timings[MEASURED_PASSES - 1],
    );
}

#[tokio::test]
#[ignore = "benchmark; run explicitly in release"]
async fn decode_bench_int_varchar() {
    measure("int_varchar(#238)", schema_int_varchar(), Sink::Discard).await;
    measure("int_varchar(#238)", schema_int_varchar(), Sink::Materialize).await;
}

#[tokio::test]
#[ignore = "benchmark; run explicitly in release"]
async fn decode_bench_mixed() {
    measure("mixed_shapes", schema_mixed(), Sink::Discard).await;
    measure("mixed_shapes", schema_mixed(), Sink::Materialize).await;
}

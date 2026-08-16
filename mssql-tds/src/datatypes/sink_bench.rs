// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Timing harness for the [`RowWriter`] value-sink path (#253).
//!
//! Every entry point is `#[ignore]`d. These are measurements, not assertions:
//! they run for minutes and their output is a table for a human, so they must
//! not join the normal suite. Run one with
//!
//! ```text
//! cargo test --release -p mssql-tds --lib sink_bench -- --ignored --nocapture
//! ```
//!
//! Rows are fed to a real [`NetworkTransport`] over a `tokio::io::duplex` pair
//! and framed into 4096-byte TDS packets, so packet header parsing, buffered
//! reads, and the token dispatch that surround `decode_into` are all present
//! rather than stubbed. `receive_row_into` is the same entry point the client's
//! row loop uses.
//!
//! The whole stream is written before the first read. That makes every run
//! decode-bound and repeatable, but it also means these numbers are an upper
//! bound: against a real server the win is diluted by whatever fraction of the
//! time is spent waiting on the socket. Feeding the duplex in pieces instead is
//! bistable on a `current_thread` runtime — the feeder and the reader settle
//! into one of two interleavings per run — and the resulting spread swamps the
//! effect being measured.
//!
//! Four writers are measured against each workload:
//!
//! - [`DefaultRowWriter`] — the shipped writer. It never opts in, so this is the
//!   no-regression gate: the sink must not cost it anything.
//! - `ArenaWriter` in [`ArenaMode::Copy`] — a consumer that owns its destination
//!   but has to take values through `write_bytes`/`write_string`, so `mssql-tds`
//!   allocates a `Vec` per value and the consumer copies out of it and drops it.
//!   This is what a PostgreSQL FDW does today.
//! - `ArenaWriter` in [`ArenaMode::Sink`] — the same consumer opting in, so the
//!   payload lands in its buffer straight off the wire. Copy vs. Sink on one
//!   binary is the measurement this design exists for.
//! - [`DiscardRowWriter`] — decode with no writer work at all.
//!
//! [`DiscardRowWriter`] is not a floor. It does not opt in either, so a value
//! it throws away still costs `mssql-tds` an allocation and a zero-fill, which
//! is why the sink arm can legitimately beat it.
//!
//! `bench_narrow_control` is the negative control: `varchar(6)` is not PLP, so
//! no destination is offered and sink must equal copy. The three PLP workloads
//! are where the effect should appear.
//!
//! Only compare arms within one run. Two binaries built from the same source
//! with a different crate hash move by ~2% on the narrow workload, so smaller
//! cross-binary differences than that carry no information.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::connection::transport::network_transport::NetworkTransport;
use crate::datatypes::column_values::{
    SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
    SqlSmallMoney, SqlTime, SqlXml,
};
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::row_writer::{DefaultRowWriter, DiscardRowWriter, RowWriter, ValueKind};
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_string::SqlString;
use crate::datatypes::sql_vector::SqlVector;
use crate::datatypes::sqldatatypes::{PartialLengthType, TdsDataType, TypeInfo, TypeInfoVariant};
use crate::io::token_stream::{ColumnPolicy, ParserContext, RowReadResult};
use crate::message::messages::PacketType;
use crate::query::metadata::ColumnMetadata;
use crate::test_packet_support::create_network_transport_with_data;
use crate::token::tokens::{ColMetadataToken, SqlCollation, TokenType};

/// TDS packet size to frame the synthetic stream at. 4096 is what a default
/// connection negotiates, so it is also the packet boundary density production
/// sees.
const PACKET_SIZE: usize = 4096;
const PACKET_HEADER: usize = 8;

const WARMUPS: usize = 3;
const SAMPLES: usize = 9;

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArenaMode {
    /// Decline the sink; take values through `write_bytes`/`write_string`.
    Copy,
    /// Accept the sink and let the decoder fill the arena directly.
    Sink,
}

/// Models a consumer that owns per-row destination storage — a PostgreSQL FDW
/// building a tuple in `palloc`ed memory, an Arrow builder filling a buffer.
///
/// The arena is allocated once, at construction, and the cursor resets on every
/// `end_row`, which is what a per-tuple allocation looks like from the decoder's
/// side. Allocating it up front matters: a writer that grew a `Vec` per value
/// would fold an allocation *and* a zero-fill into the measurement and hide the
/// thing being measured.
struct ArenaWriter {
    mode: ArenaMode,
    arena: Vec<u8>,
    cursor: usize,
    values: Vec<(u32, u32)>,
    pending: Option<usize>,
}

impl ArenaWriter {
    fn new(mode: ArenaMode, row_capacity: usize, columns: usize) -> Self {
        Self {
            mode,
            arena: vec![0u8; row_capacity],
            cursor: 0,
            values: Vec::with_capacity(columns),
            pending: None,
        }
    }

    fn take(&mut self, length: usize) -> usize {
        let start = self.cursor;
        self.cursor += length;
        start
    }

    fn copy_in(&mut self, bytes: &[u8]) {
        let start = self.take(bytes.len());
        self.arena[start..start + bytes.len()].copy_from_slice(bytes);
        self.values
            .push((start as u32, u32::try_from(bytes.len()).unwrap_or(u32::MAX)));
    }
}

impl RowWriter for ArenaWriter {
    fn value_destination<'a>(
        &'a mut self,
        _col: usize,
        _kind: ValueKind<'_>,
        length: usize,
    ) -> Option<&'a mut [u8]> {
        if self.mode == ArenaMode::Copy {
            return None;
        }
        let start = self.take(length);
        self.pending = Some(start);
        Some(&mut self.arena[start..start + length])
    }

    fn commit_value(&mut self, _col: usize, complete: bool) {
        let start = self.pending.take().expect("commit without destination");
        if complete {
            let length = self.cursor - start;
            self.values
                .push((start as u32, u32::try_from(length).unwrap_or(u32::MAX)));
        } else {
            self.cursor = start;
        }
    }

    fn write_bytes(&mut self, _col: usize, val: Vec<u8>) {
        self.copy_in(&val);
    }

    fn write_string(&mut self, _col: usize, val: SqlString) {
        self.copy_in(&val.bytes);
    }

    fn write_i32(&mut self, _col: usize, val: i32) {
        let start = self.take(4);
        self.arena[start..start + 4].copy_from_slice(&val.to_le_bytes());
        self.values.push((start as u32, 4));
    }

    fn end_row(&mut self) {
        self.cursor = 0;
        self.values.clear();
    }

    fn write_null(&mut self, _col: usize) {}
    fn write_bool(&mut self, _col: usize, _val: bool) {}
    fn write_u8(&mut self, _col: usize, _val: u8) {}
    fn write_i16(&mut self, _col: usize, _val: i16) {}
    fn write_i64(&mut self, _col: usize, _val: i64) {}
    fn write_f32(&mut self, _col: usize, _val: f32) {}
    fn write_f64(&mut self, _col: usize, _val: f64) {}
    fn write_decimal(&mut self, _col: usize, _val: DecimalParts) {}
    fn write_numeric(&mut self, _col: usize, _val: DecimalParts) {}
    fn write_date(&mut self, _col: usize, _val: SqlDate) {}
    fn write_time(&mut self, _col: usize, _val: SqlTime) {}
    fn write_datetime(&mut self, _col: usize, _val: SqlDateTime) {}
    fn write_smalldatetime(&mut self, _col: usize, _val: SqlSmallDateTime) {}
    fn write_datetime2(&mut self, _col: usize, _val: SqlDateTime2) {}
    fn write_datetimeoffset(&mut self, _col: usize, _val: SqlDateTimeOffset) {}
    fn write_money(&mut self, _col: usize, _val: SqlMoney) {}
    fn write_smallmoney(&mut self, _col: usize, _val: SqlSmallMoney) {}
    fn write_uuid(&mut self, _col: usize, _val: Uuid) {}
    fn write_xml(&mut self, _col: usize, _val: SqlXml) {}
    fn write_json(&mut self, _col: usize, _val: SqlJson) {}
    fn write_vector(&mut self, _col: usize, _val: SqlVector) {}
}

// ---------------------------------------------------------------------------
// Stream construction
// ---------------------------------------------------------------------------

fn collation() -> SqlCollation {
    SqlCollation {
        info: 0x0409_0034,
        lcid_language_id: 0x0409,
        col_flags: 0,
        sort_id: 52,
    }
}

fn plp_column(
    name: &str,
    data_type: TdsDataType,
    partial_type: PartialLengthType,
    collation: Option<SqlCollation>,
) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0,
        data_type,
        type_info: TypeInfo {
            tds_type: data_type,
            length: 0xFFFF,
            type_info_variant: TypeInfoVariant::PartialLen(
                partial_type,
                Some(0xFFFF),
                collation,
                None,
                None,
            ),
        },
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

fn int4_column(name: &str) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0,
        data_type: TdsDataType::Int4,
        type_info: TypeInfo::fixed_len(TdsDataType::Int4).expect("int4 type info"),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

fn varchar_column(name: &str, length: usize) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0,
        data_type: TdsDataType::BigVarChar,
        type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, length, Some(collation()))
            .expect("varchar type info"),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

/// Appends a known-length PLP value: total length, then one chunk per
/// `chunk_size` bytes, then the terminator.
fn append_plp(out: &mut Vec<u8>, payload: &[u8], chunk_size: usize) {
    out.extend_from_slice(&(payload.len() as i64).to_le_bytes());
    for chunk in payload.chunks(chunk_size) {
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
}

/// Frames `payload` into TDS packets, marking only the final packet EOM so the
/// whole stream reads as one message.
fn packetize(payload: &[u8]) -> Vec<u8> {
    let body = PACKET_SIZE - PACKET_HEADER;
    let packets = payload.len().div_ceil(body).max(1);
    let mut out = Vec::with_capacity(payload.len() + packets * PACKET_HEADER);

    let mut chunks = payload.chunks(body).peekable();
    while let Some(chunk) = chunks.next() {
        let total = u16::try_from(PACKET_HEADER + chunk.len()).expect("packet exceeds u16");
        out.push(PacketType::TabularResult as u8);
        out.push(if chunks.peek().is_none() { 0x01 } else { 0x00 });
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(chunk);
    }
    out
}

struct Workload {
    name: &'static str,
    metadata: Arc<ColMetadataToken>,
    stream: Vec<u8>,
    rows: usize,
    /// Total payload bytes decoded, for throughput reporting.
    payload_bytes: usize,
    /// Bytes an `ArenaWriter` must hold for one row.
    row_capacity: usize,
    columns: usize,
}

/// Which PLP type a workload is built from. Collapses the three metadata
/// fields that always vary together.
#[derive(Clone, Copy)]
enum PlpKind {
    VarBinaryMax,
    NVarCharMax,
}

impl PlpKind {
    fn column(self, name: &str) -> ColumnMetadata {
        match self {
            PlpKind::VarBinaryMax => plp_column(
                name,
                TdsDataType::BigVarBinary,
                PartialLengthType::BigVarBinary,
                None,
            ),
            PlpKind::NVarCharMax => plp_column(
                name,
                TdsDataType::NVarChar,
                PartialLengthType::NVarChar,
                Some(collation()),
            ),
        }
    }
}

/// `columns` PLP values of `value_bytes` each, repeated for `rows` rows.
fn plp_workload(
    name: &'static str,
    kind: PlpKind,
    columns: usize,
    value_bytes: usize,
    rows: usize,
    chunk_size: usize,
) -> Workload {
    let metadata = Arc::new(ColMetadataToken {
        column_count: columns as u16,
        columns: (0..columns)
            .map(|i| kind.column(&format!("c{i}")))
            .collect(),
        cek_table: vec![],
    });

    // A repeating non-uniform payload: uniform bytes would let a memcpy-shaped
    // workload look better than it is on some allocators.
    let payload: Vec<u8> = (0..value_bytes).map(|i| (i % 251) as u8).collect();

    let mut body = Vec::new();
    for _ in 0..rows {
        body.push(TokenType::Row as u8);
        for _ in 0..columns {
            append_plp(&mut body, &payload, chunk_size);
        }
    }

    Workload {
        name,
        metadata,
        stream: packetize(&body),
        rows,
        payload_bytes: columns * value_bytes * rows,
        row_capacity: columns * value_bytes,
        columns,
    }
}

/// The negative control: a narrow row of fixed-length ints and short non-PLP
/// strings. Nothing here can use the sink, so any movement is the cost of
/// asking.
fn narrow_workload(rows: usize) -> Workload {
    const INTS: usize = 39;
    const STRINGS: usize = 9;
    const STRING_LEN: usize = 6;

    let mut columns: Vec<ColumnMetadata> =
        (0..INTS).map(|i| int4_column(&format!("i{i}"))).collect();
    columns.extend((0..STRINGS).map(|i| varchar_column(&format!("s{i}"), STRING_LEN)));

    let metadata = Arc::new(ColMetadataToken {
        column_count: columns.len() as u16,
        columns,
        cek_table: vec![],
    });

    let mut body = Vec::new();
    for row in 0..rows {
        body.push(TokenType::Row as u8);
        for i in 0..INTS {
            body.extend_from_slice(&((row * INTS + i) as i32).to_le_bytes());
        }
        for _ in 0..STRINGS {
            body.extend_from_slice(&(STRING_LEN as u16).to_le_bytes());
            body.extend_from_slice(b"abcdef");
        }
    }

    Workload {
        name: "narrow 39 int + 9 varchar(6)",
        metadata,
        stream: packetize(&body),
        rows,
        payload_bytes: rows * (INTS * 4 + STRINGS * STRING_LEN),
        row_capacity: INTS * 4 + STRINGS * STRING_LEN,
        columns: INTS + STRINGS,
    }
}

// ---------------------------------------------------------------------------
// Driving
// ---------------------------------------------------------------------------

/// Reads every row of `workload` into `writer` and returns how long the row
/// loop took, including the `end_row` and the per-row hook the client drives
/// after each row. Transport construction is outside the timed region.
///
/// The transport is handed the whole stream up front rather than a throttled
/// drip. Backpressure from a real socket makes the reader's await pattern
/// depend on how the feeding task happens to interleave, which on a
/// single-threaded runtime is bistable and swamps the difference between
/// writers. Feeding everything up front makes these runs decode-bound, so they
/// upper-bound the win a network-bound consumer would see.
async fn drive<W, F>(workload: &Workload, writer: &mut W, mut after_row: F) -> Duration
where
    W: RowWriter + Send,
    F: FnMut(&mut W),
{
    let mut transport: NetworkTransport = create_network_transport_with_data(&workload.stream);
    let context = ParserContext::ColumnMetadata(Arc::clone(&workload.metadata), None);

    let start = Instant::now();
    for row in 0..workload.rows {
        let result = transport
            .receive_row_into(
                &context,
                None,
                None,
                ColumnPolicy::DecodeAll,
                writer as &mut (dyn RowWriter + Send),
            )
            .await
            .unwrap_or_else(|err| panic!("{}: row {row} failed: {err:?}", workload.name));
        match result {
            RowReadResult::RowWritten => {
                writer.end_row();
                after_row(writer);
            }
            other => panic!("{}: row {row} returned {other:?}", workload.name),
        }
    }
    let elapsed = start.elapsed();
    black_box(&mut *writer);
    elapsed
}

#[derive(Clone, Copy)]
enum Arm {
    Default,
    ArenaCopy,
    ArenaSink,
    Discard,
}

impl Arm {
    fn label(self) -> &'static str {
        match self {
            Arm::Default => "DefaultRowWriter",
            Arm::ArenaCopy => "Arena (copy)",
            Arm::ArenaSink => "Arena (sink)",
            Arm::Discard => "DiscardRowWriter",
        }
    }
}

async fn sample(workload: &Workload, arm: Arm) -> Duration {
    match arm {
        // `take_row` is what `next_row()` does after every row. Without it the
        // writer retains every value it was handed and the arm measures heap
        // growth instead of decode.
        Arm::Default => {
            let mut writer = DefaultRowWriter::new(workload.columns);
            drive(workload, &mut writer, |w| {
                black_box(w.take_row());
            })
            .await
        }
        Arm::ArenaCopy => {
            let mut writer =
                ArenaWriter::new(ArenaMode::Copy, workload.row_capacity, workload.columns);
            drive(workload, &mut writer, |_| {}).await
        }
        Arm::ArenaSink => {
            let mut writer =
                ArenaWriter::new(ArenaMode::Sink, workload.row_capacity, workload.columns);
            drive(workload, &mut writer, |_| {}).await
        }
        Arm::Discard => {
            let mut writer = DiscardRowWriter;
            drive(workload, &mut writer, |_| {}).await
        }
    }
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

/// Runs every arm against `workload` `rounds` times, alternating arms within a
/// round so drift affects all of them equally, and prints median-of-medians
/// with the per-round spread.
async fn run(workload: &Workload, rounds: usize) {
    let arms = [Arm::Default, Arm::ArenaCopy, Arm::ArenaSink, Arm::Discard];

    println!(
        "\n=== {} | {} rows | {:.1} MiB payload | {} cols ===",
        workload.name,
        workload.rows,
        workload.payload_bytes as f64 / (1024.0 * 1024.0),
        workload.columns,
    );

    for arm in arms {
        for _ in 0..WARMUPS {
            sample(workload, arm).await;
        }
    }

    let mut round_medians: Vec<Vec<Duration>> = vec![Vec::new(); arms.len()];
    for _ in 0..rounds {
        for (index, arm) in arms.iter().enumerate() {
            let mut samples = Vec::with_capacity(SAMPLES);
            for _ in 0..SAMPLES {
                samples.push(sample(workload, *arm).await);
            }
            round_medians[index].push(median(samples));
        }
    }

    let finals: Vec<Duration> = round_medians
        .iter()
        .map(|rounds| median(rounds.clone()))
        .collect();
    let baseline = finals[0].as_secs_f64();

    for (index, arm) in arms.iter().enumerate() {
        let seconds = finals[index].as_secs_f64();
        let throughput = workload.payload_bytes as f64 / seconds / (1024.0 * 1024.0);
        let spread: Vec<String> = round_medians[index]
            .iter()
            .map(|d| format!("{:.1}", d.as_secs_f64() * 1000.0))
            .collect();
        println!(
            "{:<18} {:>9.2} ms  {:>8.1} MiB/s  {:>+7.1}% vs Default   rounds=[{}]",
            arm.label(),
            seconds * 1000.0,
            throughput,
            (seconds / baseline - 1.0) * 100.0,
            spread.join(", "),
        );
    }

    let copy = finals[1].as_secs_f64();
    let sink = finals[2].as_secs_f64();
    println!(
        "sink vs copy: {:+.1}%  ({} -> {} MiB/s)",
        (sink / copy - 1.0) * 100.0,
        format_args!(
            "{:.1}",
            workload.payload_bytes as f64 / copy / (1024.0 * 1024.0)
        ),
        format_args!(
            "{:.1}",
            workload.payload_bytes as f64 / sink / (1024.0 * 1024.0)
        ),
    );
}

const ROUNDS: usize = 4;

#[tokio::test(flavor = "current_thread")]
#[ignore = "timing harness; run explicitly with --ignored --nocapture"]
async fn bench_varbinary_max_wide() {
    let workload = plp_workload(
        "varbinary(max) 4 x 64 KiB",
        PlpKind::VarBinaryMax,
        4,
        64 * 1024,
        256,
        8 * 1024,
    );
    run(&workload, ROUNDS).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "timing harness; run explicitly with --ignored --nocapture"]
async fn bench_nvarchar_max_wide() {
    let workload = plp_workload(
        "nvarchar(max) 4 x 64 KiB",
        PlpKind::NVarCharMax,
        4,
        64 * 1024,
        256,
        8 * 1024,
    );
    run(&workload, ROUNDS).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "timing harness; run explicitly with --ignored --nocapture"]
async fn bench_varbinary_max_small() {
    let workload = plp_workload(
        "varbinary(max) 24 x 128 B",
        PlpKind::VarBinaryMax,
        24,
        128,
        20_000,
        128,
    );
    run(&workload, ROUNDS).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "timing harness; run explicitly with --ignored --nocapture"]
async fn bench_narrow_control() {
    let workload = narrow_workload(30_000);
    run(&workload, ROUNDS).await;
}

/// Guards the harness itself: if the synthetic stream stops decoding, or the
/// sink and copy arms stop agreeing on the bytes they receive, every number the
/// `#[ignore]`d benches print is meaningless. Small enough to stay in the
/// normal suite.
#[tokio::test]
async fn harness_sink_and_copy_agree() {
    let workload = plp_workload("self-check", PlpKind::VarBinaryMax, 3, 5000, 4, 1024);

    let mut copy = ArenaWriter::new(ArenaMode::Copy, workload.row_capacity, workload.columns);
    let mut sink = ArenaWriter::new(ArenaMode::Sink, workload.row_capacity, workload.columns);

    drive(&workload, &mut copy, |_| {}).await;
    let copied = copy.arena.clone();
    drive(&workload, &mut sink, |_| {}).await;

    assert_eq!(
        copied, sink.arena,
        "sink and copy paths must deliver identical bytes"
    );
    let expected: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        &sink.arena[..5000],
        &expected[..],
        "payload must round-trip"
    );
}

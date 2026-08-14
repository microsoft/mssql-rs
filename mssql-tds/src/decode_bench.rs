// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Throwaway row-decode microbenchmark used to measure the E1–E6 feasibility
//! spikes against their merge-base. Lives inside the crate (rather than in
//! `benches/`) because the row decode entry point and `TdsPacketReader` are
//! `pub(crate)` and a Criterion bench is a separate crate.
//!
//! Run with:
//! ```text
//! cargo nextest run --release -p mssql-tds --lib decode_bench --no-capture
//! ```
//!
//! This file is intentionally identical between the baseline worktree and the
//! feasibility branch except for the `#[async_trait]` attribute on the reader
//! impl, which the trait's own shape forces.

use std::future::poll_fn;
use std::hint::black_box;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use crate::core::{CancelHandle, TdsResult};
use crate::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::row_writer::{DefaultRowWriter, LenHint, RowWriter, ValueKind};
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_string::EncodingType;
use crate::datatypes::sql_vector::SqlVector;
use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfo};
use crate::io::packet_reader::TdsPacketReader;
use crate::io::token_stream::{
    ColumnPolicy, GenericTokenParserRegistry, ParserContext, receive_row_into_internal,
};
use crate::query::metadata::ColumnMetadata;
use crate::token::tokens::{ColMetadataToken, SqlCollation, TokenType};
use uuid::Uuid;

/// Rows decoded per timed pass.
const ROWS: usize = 20_000;
/// Untimed passes before measurement, to settle caches and branch predictors.
const WARMUP_PASSES: usize = 3;
/// Timed passes; the reported figure is the median.
const MEASURED_PASSES: usize = 9;

// ---------------------------------------------------------------------------
// In-memory packet reader
// ---------------------------------------------------------------------------

/// Serves a pre-built byte buffer with no I/O, so a measurement reflects decode
/// cost rather than socket or syscall behavior.
struct MemReader {
    data: Arc<Vec<u8>>,
    pos: usize,
}

impl MemReader {
    fn new(data: Arc<Vec<u8>>) -> Self {
        Self { data, pos: 0 }
    }

    #[inline]
    fn take(&mut self, n: usize) -> TdsResult<&[u8]> {
        let end = self.pos + n;
        if end > self.data.len() {
            return Err(crate::error::Error::ProtocolError(
                "unexpected end of bench buffer".to_string(),
            ));
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }
}

impl TdsPacketReader for MemReader {
    async fn read_byte(&mut self) -> TdsResult<u8> {
        Ok(self.take(1)?[0])
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        let r = self.take(2)?;
        Ok(i16::from_be_bytes([r[0], r[1]]))
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        let r = self.take(4)?;
        Ok(i32::from_be_bytes([r[0], r[1], r[2], r[3]]))
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        let r = self.take(5)?;
        Ok(u64::from(r[0])
            | u64::from(r[1]) << 8
            | u64::from(r[2]) << 16
            | u64::from(r[3]) << 24
            | u64::from(r[4]) << 32)
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        let r = self.take(4)?;
        Ok(f32::from_le_bytes([r[0], r[1], r[2], r[3]]))
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        let r = self.take(8)?;
        Ok(f64::from_le_bytes([
            r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7],
        ]))
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        let r = self.take(2)?;
        Ok(i16::from_le_bytes([r[0], r[1]]))
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        let r = self.take(2)?;
        Ok(u16::from_le_bytes([r[0], r[1]]))
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        let r = self.take(3)?;
        Ok(u32::from(r[0]) | u32::from(r[1]) << 8 | u32::from(r[2]) << 16)
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        let r = self.take(4)?;
        Ok(i32::from_le_bytes([r[0], r[1], r[2], r[3]]))
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        let r = self.take(4)?;
        Ok(u32::from_le_bytes([r[0], r[1], r[2], r[3]]))
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        let r = self.take(8)?;
        Ok(i64::from_le_bytes([
            r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7],
        ]))
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        let r = self.take(8)?;
        Ok(u64::from_le_bytes([
            r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7],
        ]))
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        let n = buffer.len();
        buffer.copy_from_slice(self.take(n)?);
        Ok(n)
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let n = self.take(1)?[0] as usize;
        Ok(self.take(n)?.to_vec())
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let r = self.take(2)?;
        let n = u16::from_le_bytes([r[0], r[1]]) as usize;
        Ok(self.take(n)?.to_vec())
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        let r = self.take(2)?;
        let n = u16::from_le_bytes([r[0], r[1]]);
        if n == crate::io::packet_reader::LENGTH_NULL {
            return Ok(None);
        }
        Ok(Some(self.read_unicode(n as usize).await?))
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        let n = self.take(1)?[0] as usize;
        self.read_unicode(n).await
    }

    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
        self.read_unicode_with_byte_length(string_length * 2).await
    }

    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String> {
        let raw = self.take(byte_length)?;
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&units))
    }

    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()> {
        self.take(skip_count)?;
        Ok(())
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        Ok(())
    }

    fn reset_reader(&mut self) {
        self.pos = 0;
    }
}

// ---------------------------------------------------------------------------
// Synthetic result-set construction
// ---------------------------------------------------------------------------

fn collation() -> SqlCollation {
    SqlCollation {
        info: 0x0000_0409,
        lcid_language_id: 0x0409,
        col_flags: 0,
        sort_id: 52,
    }
}

/// One column's shape. Metadata and wire bytes are both derived from this, so
/// they cannot drift apart.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ColSpec {
    /// `INT`, sent as `IntN` with a 4-byte payload.
    Int,
    /// `VARCHAR(n)`, sent as non-PLP `BigVarChar`.
    Str(usize),
}

impl ColSpec {
    fn metadata(self, name: String) -> ColumnMetadata {
        match self {
            ColSpec::Int => int_column(&name),
            ColSpec::Str(len) => varchar_column(&name, len),
        }
    }
}

fn int_column(name: &str) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0x01, // nullable
        data_type: TdsDataType::IntN,
        type_info: TypeInfo::var_len(TdsDataType::IntN, 4).unwrap(),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

fn varchar_column(name: &str, len: usize) -> ColumnMetadata {
    ColumnMetadata {
        user_type: 0,
        flags: 0x01, // nullable
        data_type: TdsDataType::BigVarChar,
        type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, len, Some(collation()))
            .unwrap(),
        column_name: name.to_string(),
        multi_part_name: None,
        crypto_metadata: None,
    }
}

/// Column layout of the PoC's benchmark table: 39 `INT` + 9 `VARCHAR(6)`,
/// all nullable.
fn poc_columns() -> Vec<ColSpec> {
    let mut cols = vec![ColSpec::Int; 39];
    cols.extend(std::iter::repeat_n(ColSpec::Str(6), 9));
    cols
}

/// Narrower layout with large string payloads, to weight the value-handoff path
/// (E3) rather than per-column dispatch.
fn wide_string_columns() -> Vec<ColSpec> {
    vec![ColSpec::Str(512); 8]
}

fn context_for(specs: &[ColSpec]) -> ParserContext {
    let columns: Vec<ColumnMetadata> = specs
        .iter()
        .enumerate()
        .map(|(i, spec)| spec.metadata(format!("col_{i}")))
        .collect();
    ParserContext::ColumnMetadata(
        Arc::new(ColMetadataToken {
            column_count: columns.len() as u16,
            columns,
            ..Default::default()
        }),
        None,
    )
}

/// Encodes one column value onto the wire exactly as the server would.
fn push_value(buf: &mut Vec<u8>, spec: ColSpec, row: usize, col: usize) {
    match spec {
        ColSpec::Int => {
            buf.push(4);
            buf.extend_from_slice(&((row * 48 + col) as i32).to_le_bytes());
        }
        ColSpec::Str(len) => {
            buf.extend_from_slice(&(len as u16).to_le_bytes());
            buf.extend((0..len).map(|i| b'a' + ((i + col) % 26) as u8));
        }
    }
}

/// Builds `ROWS` ROW tokens with every column present.
fn build_row_stream(specs: &[ColSpec]) -> Vec<u8> {
    let mut buf = Vec::new();
    for row in 0..ROWS {
        buf.push(TokenType::Row as u8);
        for (col, spec) in specs.iter().enumerate() {
            push_value(&mut buf, *spec, row, col);
        }
    }
    buf
}

/// Builds `ROWS` NBCROW tokens where every 4th column is NULL, so the null
/// bitmap is both present and non-trivial.
fn build_nbcrow_stream(specs: &[ColSpec]) -> Vec<u8> {
    let bitmap_len = specs.len().div_ceil(8);
    let mut buf = Vec::new();
    for row in 0..ROWS {
        buf.push(TokenType::NbcRow as u8);
        let mut bitmap = vec![0u8; bitmap_len];
        for col in 0..specs.len() {
            if col % 4 == 3 {
                bitmap[col / 8] |= 1 << (col % 8);
            }
        }
        buf.extend_from_slice(&bitmap);
        for (col, spec) in specs.iter().enumerate() {
            if col % 4 != 3 {
                push_value(&mut buf, *spec, row, col);
            }
        }
    }
    buf
}

// ---------------------------------------------------------------------------
// Contiguous-buffer writer
// ---------------------------------------------------------------------------

/// Mirrors the shape of `mssql-js`'s `BinaryRowWriter`: every value is appended
/// into one reusable byte buffer rather than becoming an owned `ColumnValues`.
///
/// `DefaultRowWriter` cannot show E3's benefit because it allocates a fresh
/// `Vec` per value either way. This writer can: on the accumulator API the
/// decoder writes straight into `buf`, whereas the old API forces the decoder to
/// assemble a temporary `Vec` first and hand it over to be copied again.
#[derive(Default)]
struct ContiguousRowWriter {
    buf: Vec<u8>,
    row_start: usize,
    len_at: usize,
}

impl ContiguousRowWriter {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(64 * 1024),
            row_start: 0,
            len_at: 0,
        }
    }
}

/// Generates the fixed-width writers the benchmark does not exercise. They must
/// exist to satisfy the trait but never run, so a uniform body is enough.
macro_rules! unused_writers {
    ($($name:ident($ty:ty)),* $(,)?) => {
        $(fn $name(&mut self, _col: usize, val: $ty) { black_box(&val); })*
    };
}

impl RowWriter for ContiguousRowWriter {
    fn write_null(&mut self, _col: usize) {
        self.buf.push(0);
    }

    fn write_i32(&mut self, _col: usize, val: i32) {
        self.buf.push(1);
        self.buf.extend_from_slice(&val.to_le_bytes());
    }

    fn write_str(&mut self, _col: usize, bytes: &[u8], _encoding: EncodingType) {
        self.buf.push(2);
        self.buf
            .extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(bytes);
    }

    fn write_blob(&mut self, _col: usize, bytes: &[u8]) {
        self.write_str(_col, bytes, EncodingType::Utf8);
    }

    fn begin_value(&mut self, _col: usize, kind: ValueKind, hint: LenHint) {
        self.buf.push(match kind {
            ValueKind::Str(_) => 2,
            ValueKind::Blob => 3,
        });
        self.len_at = self.buf.len();
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        if let LenHint::Exact(n) = hint {
            self.buf.reserve(n);
        }
    }

    fn reserve(&mut self, _col: usize, n: usize) -> &mut [u8] {
        let at = self.buf.len();
        self.buf.resize(at + n, 0);
        &mut self.buf[at..]
    }

    fn commit_value(&mut self, _col: usize) {
        let len = (self.buf.len() - self.len_at - 4) as u32;
        self.buf[self.len_at..self.len_at + 4].copy_from_slice(&len.to_le_bytes());
    }

    fn end_row(&mut self) {
        // Stands in for handing the encoded row to the host runtime.
        self.buf.clear();
        self.row_start = 0;
    }

    fn abandon_row(&mut self) {
        self.buf.truncate(self.row_start);
    }

    unused_writers!(
        write_bool(bool),
        write_u8(u8),
        write_i16(i16),
        write_i64(i64),
        write_f32(f32),
        write_f64(f64),
        write_decimal(DecimalParts),
        write_numeric(DecimalParts),
        write_date(SqlDate),
        write_time(SqlTime),
        write_datetime(SqlDateTime),
        write_smalldatetime(SqlSmallDateTime),
        write_datetime2(SqlDateTime2),
        write_datetimeoffset(SqlDateTimeOffset),
        write_money(SqlMoney),
        write_smallmoney(SqlSmallMoney),
        write_uuid(Uuid),
        write_xml(SqlXml),
        write_json(SqlJson),
        write_vector(SqlVector),
    );
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Decodes the whole buffer once, returning the elapsed time. The decoded values
/// are fed through `black_box` so the work cannot be optimized away.
async fn decode_pass(data: Arc<Vec<u8>>, context: &ParserContext, col_count: usize) -> Duration {
    let registry = GenericTokenParserRegistry::default();
    let mut reader = MemReader::new(data);
    let mut writer = DefaultRowWriter::new(col_count);

    let start = Instant::now();
    for _ in 0..ROWS {
        receive_row_into_internal(
            &mut reader,
            &registry,
            context,
            ColumnPolicy::DecodeAll,
            &mut writer,
        )
        .await
        .expect("row decode failed");
        black_box(writer.take_row());
    }
    start.elapsed()
}

/// Decodes one buffer and asserts the decoded values match what the builder
/// wrote, and that the stream was consumed exactly. Without this, a harness bug
/// (short reads, wrong wire shape) would silently produce fast but meaningless
/// numbers.
async fn verify_pass(data: Arc<Vec<u8>>, context: &ParserContext, specs: &[ColSpec], nbc: bool) {
    let registry = GenericTokenParserRegistry::default();
    let mut reader = MemReader::new(Arc::clone(&data));
    let mut writer = DefaultRowWriter::new(specs.len());

    for row in 0..ROWS {
        receive_row_into_internal(
            &mut reader,
            &registry,
            context,
            ColumnPolicy::DecodeAll,
            &mut writer,
        )
        .await
        .expect("row decode failed");

        let values = writer.take_row();
        assert_eq!(values.len(), specs.len(), "row {row} column count");

        for (col, spec) in specs.iter().enumerate() {
            let is_null = nbc && col % 4 == 3;
            match (&values[col], spec, is_null) {
                (ColumnValues::Null, _, true) => {}
                (ColumnValues::Int(v), ColSpec::Int, false) => {
                    assert_eq!(*v, (row * 48 + col) as i32, "row {row} col {col}");
                }
                (ColumnValues::String(s), ColSpec::Str(len), false) => {
                    let expected: Vec<u8> =
                        (0..*len).map(|i| b'a' + ((i + col) % 26) as u8).collect();
                    assert_eq!(
                        s.to_utf8_string(),
                        String::from_utf8(expected).unwrap(),
                        "row {row} col {col}"
                    );
                }
                (actual, _, _) => panic!("row {row} col {col}: unexpected value {actual:?}"),
            }
        }
    }

    assert_eq!(
        reader.pos,
        data.len(),
        "decoder did not consume the stream exactly"
    );
}

async fn decode_pass_contiguous(
    data: Arc<Vec<u8>>,
    context: &ParserContext,
    _col_count: usize,
) -> Duration {
    let registry = GenericTokenParserRegistry::default();
    let mut reader = MemReader::new(data);
    let mut writer = ContiguousRowWriter::new();

    let start = Instant::now();
    for _ in 0..ROWS {
        receive_row_into_internal(
            &mut reader,
            &registry,
            context,
            ColumnPolicy::DecodeAll,
            &mut writer,
        )
        .await
        .expect("row decode failed");
        black_box(&writer.buf);
        writer.end_row();
    }
    start.elapsed()
}

/// Which of the production wrappers to apply around the row decode.
///
/// `NetworkTransport::receive_row_into` wraps `receive_row_into_internal` in
/// `CancelHandle::run_until_cancelled` and then optionally `tokio::time::timeout`.
/// The harness normally bypasses both, so this ladder exists to price them.
#[derive(Clone, Copy, Debug)]
enum WrapMode {
    /// Direct call, as the rest of this harness does.
    Bare,
    /// Wrapped, but with no cancel handle supplied — the `None => f.await` arm.
    CancelNone,
    /// Wrapped with a live cancel handle that never fires.
    CancelSome,
    /// Full production shape: cancel handle plus a request timeout.
    CancelSomeTimeout,
    /// Proposed shape: same budget, but the timer is armed only if the decode
    /// actually suspends. Polls the inner future once first; a row served from
    /// an already-buffered packet never touches the timer wheel.
    CancelSomeLazyTimeout,
    /// Same lazy arming, written inline instead of behind an `async fn`, to
    /// price the wrapper future that the helper itself introduces.
    CancelSomeLazyInline,
}

impl WrapMode {
    fn label(self) -> &'static str {
        match self {
            WrapMode::Bare => "bare",
            WrapMode::CancelNone => "cancel_none",
            WrapMode::CancelSome => "cancel_some",
            WrapMode::CancelSomeTimeout => "cancel_some_timeout",
            WrapMode::CancelSomeLazyTimeout => "cancel_some_lazy_timeout",
            WrapMode::CancelSomeLazyInline => "cancel_some_lazy_inline",
        }
    }
}

/// Arms a `tokio::time::timeout` only if `f` does not complete on its first poll.
///
/// Observationally equivalent to `tokio::time::timeout` for this path:
/// `Timeout::poll` polls the inner future before the delay, so a future that is
/// ready immediately never observes the timer either way. The difference is that
/// this version never registers a timer-wheel entry in that case.
async fn lazy_timeout<F, T>(dur: Duration, f: F) -> Result<T, tokio::time::error::Elapsed>
where
    F: Future<Output = T>,
{
    let mut f = pin!(f);
    match poll_fn(|cx| Poll::Ready(f.as_mut().poll(cx))).await {
        Poll::Ready(v) => Ok(v),
        Poll::Pending => tokio::time::timeout(dur, f).await,
    }
}

async fn decode_pass_wrapped(
    data: Arc<Vec<u8>>,
    context: &ParserContext,
    mode: WrapMode,
    handle: &CancelHandle,
) -> Duration {
    let registry = GenericTokenParserRegistry::default();
    let mut reader = MemReader::new(data);
    let mut writer = ContiguousRowWriter::new();
    // Long enough that it can never fire; we are pricing the wrapper, not the timer.
    let request_timeout = Duration::from_secs(600);

    let start = Instant::now();
    for _ in 0..ROWS {
        let result = match mode {
            WrapMode::Bare => {
                receive_row_into_internal(
                    &mut reader,
                    &registry,
                    context,
                    ColumnPolicy::DecodeAll,
                    &mut writer,
                )
                .await
            }
            WrapMode::CancelNone => {
                CancelHandle::run_until_cancelled(
                    None,
                    receive_row_into_internal(
                        &mut reader,
                        &registry,
                        context,
                        ColumnPolicy::DecodeAll,
                        &mut writer,
                    ),
                )
                .await
            }
            WrapMode::CancelSome => {
                CancelHandle::run_until_cancelled(
                    Some(handle),
                    receive_row_into_internal(
                        &mut reader,
                        &registry,
                        context,
                        ColumnPolicy::DecodeAll,
                        &mut writer,
                    ),
                )
                .await
            }
            WrapMode::CancelSomeTimeout => {
                let cancellable = CancelHandle::run_until_cancelled(
                    Some(handle),
                    receive_row_into_internal(
                        &mut reader,
                        &registry,
                        context,
                        ColumnPolicy::DecodeAll,
                        &mut writer,
                    ),
                );
                match tokio::time::timeout(request_timeout, cancellable).await {
                    Ok(r) => r,
                    Err(_) => panic!("bench timeout fired"),
                }
            }
            WrapMode::CancelSomeLazyTimeout => {
                let cancellable = CancelHandle::run_until_cancelled(
                    Some(handle),
                    receive_row_into_internal(
                        &mut reader,
                        &registry,
                        context,
                        ColumnPolicy::DecodeAll,
                        &mut writer,
                    ),
                );
                match lazy_timeout(request_timeout, cancellable).await {
                    Ok(r) => r,
                    Err(_) => panic!("bench timeout fired"),
                }
            }
            WrapMode::CancelSomeLazyInline => {
                let mut cancellable = pin!(CancelHandle::run_until_cancelled(
                    Some(handle),
                    receive_row_into_internal(
                        &mut reader,
                        &registry,
                        context,
                        ColumnPolicy::DecodeAll,
                        &mut writer,
                    ),
                ));
                match poll_fn(|cx| Poll::Ready(cancellable.as_mut().poll(cx))).await {
                    Poll::Ready(r) => r,
                    Poll::Pending => match tokio::time::timeout(request_timeout, cancellable).await
                    {
                        Ok(r) => r,
                        Err(_) => panic!("bench timeout fired"),
                    },
                }
            }
        };
        result.expect("row decode failed");
        black_box(&writer.buf);
        writer.end_row();
    }
    start.elapsed()
}

/// Prices the per-row cancellation/timeout wrappers that the rest of this
/// harness bypasses.
///
/// **This design cannot separate a mode's cost from machine drift.** It runs
/// every pass of one mode before moving to the next, so any drift across the
/// sequence lands entirely on mode order. Sharing one runtime does not make the
/// comparison paired. Use [`run_paired_wrapper_ab`] for any figure that is
/// quoted; this remains only as the survey that suggests which pair to test.
fn run_wrapper_ladder(name: &str, specs: Vec<ColSpec>, nbc: bool) {
    let col_count = specs.len();
    let data = Arc::new(if nbc {
        build_nbcrow_stream(&specs)
    } else {
        build_row_stream(&specs)
    });
    let context = context_for(&specs);

    // `timeout` needs the time driver, which the other cases do not enable.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let handle = CancelHandle::new();

    for mode in [
        WrapMode::Bare,
        WrapMode::CancelNone,
        WrapMode::CancelSome,
        WrapMode::CancelSomeTimeout,
        WrapMode::CancelSomeLazyTimeout,
        WrapMode::CancelSomeLazyInline,
    ] {
        for _ in 0..WARMUP_PASSES {
            rt.block_on(decode_pass_wrapped(
                Arc::clone(&data),
                &context,
                mode,
                &handle,
            ));
        }

        let mut samples: Vec<Duration> = (0..MEASURED_PASSES)
            .map(|_| {
                rt.block_on(decode_pass_wrapped(
                    Arc::clone(&data),
                    &context,
                    mode,
                    &handle,
                ))
            })
            .collect();
        samples.sort();

        let median = samples[MEASURED_PASSES / 2];
        let ns_per_row = median.as_nanos() as f64 / ROWS as f64;

        println!(
            "BENCH\t{name}_{}\tcols={col_count}\trows={ROWS}\tmedian_ms={:.3}\t\
             ns_per_row={ns_per_row:.1}",
            mode.label(),
            median.as_secs_f64() * 1000.0,
        );
    }
}

/// Paired A/B for a single wrapper pair, with ABBA ordering.
///
/// The sample is the **difference within a pair**, so drift shared by both arms
/// cancels whether it is monotonic or not. [`run_wrapper_ladder`] cannot do
/// this: an impossible ordering there is evidence that drift *exists*, but is
/// not a bound on its magnitude, and monotonic drift would produce no impossible
/// ordering at all while still inflating whichever mode runs last.
fn run_paired_wrapper_ab(
    name: &str,
    specs: Vec<ColSpec>,
    nbc: bool,
    a: WrapMode,
    b: WrapMode,
    pairs: usize,
) {
    let col_count = specs.len();
    let data = Arc::new(if nbc {
        build_nbcrow_stream(&specs)
    } else {
        build_row_stream(&specs)
    });
    let context = context_for(&specs);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let handle = CancelHandle::new();

    for _ in 0..WARMUP_PASSES {
        rt.block_on(decode_pass_wrapped(Arc::clone(&data), &context, a, &handle));
        rt.block_on(decode_pass_wrapped(Arc::clone(&data), &context, b, &handle));
    }

    let mut diffs: Vec<i128> = Vec::with_capacity(pairs);
    let mut a_samples: Vec<Duration> = Vec::with_capacity(pairs);
    let mut b_samples: Vec<Duration> = Vec::with_capacity(pairs);

    for i in 0..pairs {
        // ABBA: alternate which arm leads so within-pair ordering bias cancels
        // across pairs as well.
        let (ta, tb) = if i % 2 == 0 {
            let ta = rt.block_on(decode_pass_wrapped(Arc::clone(&data), &context, a, &handle));
            let tb = rt.block_on(decode_pass_wrapped(Arc::clone(&data), &context, b, &handle));
            (ta, tb)
        } else {
            let tb = rt.block_on(decode_pass_wrapped(Arc::clone(&data), &context, b, &handle));
            let ta = rt.block_on(decode_pass_wrapped(Arc::clone(&data), &context, a, &handle));
            (ta, tb)
        };
        diffs.push(tb.as_nanos() as i128 - ta.as_nanos() as i128);
        a_samples.push(ta);
        b_samples.push(tb);
    }

    diffs.sort_unstable();
    a_samples.sort_unstable();
    b_samples.sort_unstable();

    let median_diff = diffs[pairs / 2];
    let b_slower = diffs.iter().filter(|d| **d > 0).count();
    let a_med = a_samples[pairs / 2];
    let b_med = b_samples[pairs / 2];
    let ns_per_row = median_diff as f64 / ROWS as f64;
    let pct_of_a = median_diff as f64 / a_med.as_nanos() as f64 * 100.0;

    println!(
        "PAIRED\t{name}\tcols={col_count}\trows={ROWS}\tpairs={pairs}\t\
         a={}\ta_med_ms={:.3}\tb={}\tb_med_ms={:.3}\t\
         median_diff_ms={:.3}\tns_per_row={ns_per_row:.1}\t\
         pct_of_a={pct_of_a:.2}\tb_slower={b_slower}/{pairs}",
        a.label(),
        a_med.as_secs_f64() * 1000.0,
        b.label(),
        b_med.as_secs_f64() * 1000.0,
        median_diff as f64 / 1e6,
    );
}

fn run_case(name: &str, specs: Vec<ColSpec>, nbc: bool) {
    let col_count = specs.len();
    let data = Arc::new(if nbc {
        build_nbcrow_stream(&specs)
    } else {
        build_row_stream(&specs)
    });
    let context = context_for(&specs);
    let bytes = data.len();

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");

    rt.block_on(verify_pass(Arc::clone(&data), &context, &specs, nbc));

    for _ in 0..WARMUP_PASSES {
        rt.block_on(decode_pass(Arc::clone(&data), &context, col_count));
    }

    let mut samples: Vec<Duration> = (0..MEASURED_PASSES)
        .map(|_| rt.block_on(decode_pass(Arc::clone(&data), &context, col_count)))
        .collect();
    samples.sort();

    let median = samples[MEASURED_PASSES / 2];
    let best = samples[0];
    let ns_per_row = median.as_nanos() as f64 / ROWS as f64;
    let rows_per_sec = ROWS as f64 / median.as_secs_f64();
    let mb_per_sec = bytes as f64 / median.as_secs_f64() / (1024.0 * 1024.0);

    println!(
        "BENCH\t{name}\tcols={col_count}\trows={ROWS}\tmedian_ms={:.3}\tbest_ms={:.3}\t\
         ns_per_row={ns_per_row:.1}\trows_per_sec={rows_per_sec:.0}\tMiB_per_sec={mb_per_sec:.1}",
        median.as_secs_f64() * 1000.0,
        best.as_secs_f64() * 1000.0,
    );
}

fn run_contiguous_case(name: &str, specs: Vec<ColSpec>, nbc: bool) {
    let col_count = specs.len();
    let data = Arc::new(if nbc {
        build_nbcrow_stream(&specs)
    } else {
        build_row_stream(&specs)
    });
    let context = context_for(&specs);
    let bytes = data.len();

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");

    for _ in 0..WARMUP_PASSES {
        rt.block_on(decode_pass_contiguous(
            Arc::clone(&data),
            &context,
            col_count,
        ));
    }

    let mut samples: Vec<Duration> = (0..MEASURED_PASSES)
        .map(|_| {
            rt.block_on(decode_pass_contiguous(
                Arc::clone(&data),
                &context,
                col_count,
            ))
        })
        .collect();
    samples.sort();

    let median = samples[MEASURED_PASSES / 2];
    let best = samples[0];
    let ns_per_row = median.as_nanos() as f64 / ROWS as f64;
    let rows_per_sec = ROWS as f64 / median.as_secs_f64();
    let mb_per_sec = bytes as f64 / median.as_secs_f64() / (1024.0 * 1024.0);

    println!(
        "BENCH\t{name}\tcols={col_count}\trows={ROWS}\tmedian_ms={:.3}\tbest_ms={:.3}\t\
         ns_per_row={ns_per_row:.1}\trows_per_sec={rows_per_sec:.0}\tMiB_per_sec={mb_per_sec:.1}",
        median.as_secs_f64() * 1000.0,
        best.as_secs_f64() * 1000.0,
    );
}

#[test]
fn bench_row_decode() {
    println!("BENCH_BEGIN");
    run_case("poc_row_39int_9varchar", poc_columns(), false);
    run_case("poc_nbcrow_39int_9varchar", poc_columns(), true);
    run_case("wide_strings_8x512", wide_string_columns(), false);
    run_contiguous_case("contig_poc_row_39int_9varchar", poc_columns(), false);
    run_contiguous_case("contig_wide_strings_8x512", wide_string_columns(), false);
    run_wrapper_ladder("wrap_poc_row_39int_9varchar", poc_columns(), false);
    for (label, a, b) in [
        // The four comparisons #271 quotes, each re-priced with a paired design.
        ("cancel_only", WrapMode::Bare, WrapMode::CancelSome),
        (
            "eager_timeout",
            WrapMode::CancelSome,
            WrapMode::CancelSomeTimeout,
        ),
        ("full_stack", WrapMode::Bare, WrapMode::CancelSomeTimeout),
        (
            "lazy_residual",
            WrapMode::CancelSome,
            WrapMode::CancelSomeLazyTimeout,
        ),
        (
            "lazy_inline_residual",
            WrapMode::CancelSome,
            WrapMode::CancelSomeLazyInline,
        ),
    ] {
        run_paired_wrapper_ab(&format!("paired_{label}"), poc_columns(), false, a, b, 41);
    }
    println!("BENCH_END");
}

/// Proves `lazy_timeout` is observationally identical to `tokio::time::timeout`
/// at the case #271 flags as a trap: an exhausted (`ZERO`) budget.
///
/// The distinction that matters is *suspension*, not the budget value. A future
/// that is ready on its first poll must succeed even on a zero budget (because
/// `Timeout::poll` polls the inner future first), while a future that suspends
/// must still fail with `Elapsed`. Skipping the timer when the budget is zero —
/// the obvious "optimization" — would invert the second case into an unbounded wait.
#[test]
fn lazy_timeout_matches_eager_timeout_on_exhausted_budget() {
    use std::future::ready;

    struct NeverReady;
    impl Future for NeverReady {
        type Output = u8;
        fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<u8> {
            Poll::Pending
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let z = Duration::ZERO;

        // Ready-on-first-poll: both succeed despite a zero budget.
        assert_eq!(tokio::time::timeout(z, ready(7u8)).await.ok(), Some(7));
        assert_eq!(lazy_timeout(z, ready(7u8)).await.ok(), Some(7));

        // Suspends: both must still elapse. This is the inversion guard.
        assert!(tokio::time::timeout(z, NeverReady).await.is_err());
        assert!(
            lazy_timeout(z, NeverReady).await.is_err(),
            "lazy arming must not turn an exhausted budget into an unbounded wait"
        );

        // A live budget still elapses on a suspending future.
        assert!(
            lazy_timeout(Duration::from_millis(20), NeverReady)
                .await
                .is_err()
        );
    });
}

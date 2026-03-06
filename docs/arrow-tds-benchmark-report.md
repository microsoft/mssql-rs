# Arrow → TDS Bulk Copy Serialization: Performance Report

## Executive Summary

This report presents benchmark results comparing two data paths for serializing
Apache Arrow columnar data into the TDS (Tabular Data Stream) wire format used
by SQL Server bulk copy operations. The **Arrow-direct pre-serialized** path
eliminates the intermediate `ColumnValues` Rust enum representation and achieves
**2.09× throughput** (362 Krows/s vs 174 Krows/s) in end-to-end SQL Server
benchmarks.

---

## 1. Background

### The Problem

When bulk-copying data from an Arrow-native source (e.g., a Pandas DataFrame,
Spark partition, or Polars table) into SQL Server, the current code path
requires converting every value from Arrow's columnar representation into a
per-row `ColumnValues` Rust enum before serializing to TDS wire bytes. This
intermediate step introduces:

- **Per-row heap allocations** (`Vec<ColumnValues>` for each row)
- **String re-encoding overhead** (Arrow UTF-8 → `SqlString` with a new `Vec<u8>`)
- **Enum dispatch cost** (match on `ColumnValues` variant during serialization)
- **Cache pressure** from scattered, per-value allocations

### The Hypothesis

By reading typed values directly from Arrow's contiguous column buffers and
writing TDS bytes in a single pass, we can eliminate the intermediate
representation and its associated overhead.

---

## 2. Experiment Design

### Three Serialization Paths Compared

```
┌─────────────────────────────────────────────────────────────────────────┐
│                                                                         │
│  PATH A: Arrow Direct                                                   │
│  ══════════════════                                                     │
│                                                                         │
│  ┌──────────────┐     ┌──────────────────┐     ┌──────────────────┐    │
│  │ Arrow Arrays  │────▶│  Read typed value │────▶│ Write TDS bytes  │    │
│  │ (columnar,    │     │  (e.g. i32, f64,  │     │ directly to buf  │    │
│  │  contiguous)  │     │   &str)           │     │                  │    │
│  └──────────────┘     └──────────────────┘     └──────────────────┘    │
│                                                                         │
│         Zero intermediate allocation per row                            │
│                                                                         │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  PATH B: Arrow Direct + Pre-encoded Strings                             │
│  ══════════════════════════════════════════                              │
│                                                                         │
│  ┌──────────────┐     ┌──────────────────┐     ┌──────────────────┐    │
│  │ Arrow Arrays  │────▶│ Batch pre-encode  │────▶│ Write TDS bytes  │    │
│  │ (columnar,    │     │ all strings to    │     │ with pre-encoded │    │
│  │  contiguous)  │     │ UTF-16LE once     │     │ string bytes     │    │
│  └──────────────┘     └──────────────────┘     └──────────────────┘    │
│                                                                         │
│         One-time batch allocation for strings; zero per-row alloc       │
│                                                                         │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  PATH C: Arrow → ColumnValues → TDS  (current code path)               │
│  ════════════════════════════════════                                    │
│                                                                         │
│  ┌──────────────┐     ┌──────────────┐     ┌──────────────┐            │
│  │ Arrow Arrays  │────▶│ Construct     │────▶│ Match on enum│            │
│  │ (columnar,    │     │ ColumnValues  │     │ variant &    │            │
│  │  contiguous)  │     │ enum per cell │     │ serialize    │            │
│  └──────────────┘     │               │     │ to TDS bytes │            │
│                        │ Vec<ColVal>   │     └──────────────┘            │
│                        │ per row       │                                 │
│                        │ SqlString     │                                 │
│                        │ alloc per str │                                 │
│                        │ DecimalParts  │                                 │
│                        │ alloc per dec │                                 │
│                        └──────────────┘                                  │
│                                                                         │
│         N×cols allocations per batch (Vec + String + Decimal)            │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### Test Data

Each row contains **5 columns** representing a realistic bulk-insert workload:

| Column | Arrow Type | TDS Wire Type | Example Value |
|--------|-----------|---------------|---------------|
| `id` | `Int32` | `INTN` (4 bytes) | `42` |
| `amount` | `Int64` | `INTN` (8 bytes) | `4242` |
| `price` | `Float64` | `FLOATN` (8 bytes) | `99.95` |
| `name` | `Utf8` | `NVARCHAR` (UTF-16LE) | `"product-name-000042"` |
| `total` | `Decimal128(18,2)` | `DECIMALN` | `123.45` |

Batch sizes tested: **10,000 rows** and **100,000 rows**.

### Methodology

- **Framework**: Criterion.rs (statistical benchmarking)
- **Warm-up**: 3 seconds per benchmark
- **Measurement**: 8 seconds, 100 samples
- **Buffer**: Pre-allocated `Vec<u8>`, reused across iterations (measures pure
  serialization, not allocation of the output buffer)
- **Environment**: Linux, Rust 2024 edition, release profile

---

## 3. Serialization-Only Results (Criterion, no SQL Server)

> **Note:** These are serialization-only micro-benchmarks. See Section 5 for the
> definitive end-to-end SQL Server results (2.09× speedup).

### 10,000 Rows

| Path | Median Time | Throughput | vs. ColumnValues |
|------|------------|-----------|-----------------|
| **Arrow Direct** | 1.03 ms | 9.7 Mrows/s | **1.19× faster** |
| **Arrow Direct (pre-encoded)** | 0.83 ms | 12.1 Mrows/s | **1.47× faster** |
| Arrow via ColumnValues | 1.22 ms | 8.2 Mrows/s | baseline |

### 100,000 Rows

| Path | Median Time | Throughput | vs. ColumnValues |
|------|------------|-----------|-----------------|
| **Arrow Direct** | 11.5 ms | 8.7 Mrows/s | **1.17× faster** |
| **Arrow Direct (pre-encoded)** | 7.8 ms | 12.9 Mrows/s | **1.72× faster** |
| Arrow via ColumnValues | 13.4 ms | 7.5 Mrows/s | baseline |

### Throughput Comparison (100K rows)

```
  Throughput (Million rows/sec)
  ─────────────────────────────────────────────────────
  0        3        6        9        12       15
  ├────────┼────────┼────────┼────────┼────────┤

  Arrow via ColumnValues
  ███████████████████████████████░░░░░░░░░░░░░░░░  7.5 Mrows/s

  Arrow Direct
  █████████████████████████████████████░░░░░░░░░░  8.7 Mrows/s  (+17%)

  Arrow Direct (pre-encoded)
  █████████████████████████████████████████████████ 12.9 Mrows/s (+72%)
```

---

## 4. Analysis: Where Does the Overhead Come From?

### Side-by-Side Data Flow (per row, 5 columns)

```
Arrow Direct (Path A)                    Arrow via ColumnValues (Path C)
═══════════════════                      ═══════════════════════════════

Arrow Int32Array                         Arrow Int32Array
  ↓ .value(row)                            ↓ .value(row)
  i32 (stack)                              i32 (stack)
  ↓ write_tds_int32()                      ↓ ColumnValues::Int(i32)     ← enum tag + copy
  [0x04][LE bytes] → buf                   ↓ match ColumnValues::Int    ← branch
                                           ↓ write_tds_int32()
                                           [0x04][LE bytes] → buf

Arrow StringArray                        Arrow StringArray
  ↓ .value(row)                            ↓ .value(row)
  &str (zero-copy ref)                     &str
  ↓ encode_utf16()                         ↓ encode_utf16()
  ↓ write directly to buf                  ↓ Vec<u8> alloc for UTF-16   ← HEAP ALLOC
                                           ↓ SqlString::new(vec, Utf16) ← enum wrapper
                                           ↓ ColumnValues::String(...)  ← enum tag + move
                                           ↓ match → .as_utf16_bytes()
                                           ↓ write to buf
                                           ↓ Vec<u8> dropped            ← DEALLOC

Arrow Decimal128Array                    Arrow Decimal128Array
  ↓ .value(row)                            ↓ .value(row)
  i128 (stack)                             i128
  ↓ write_tds_decimal128()                 ↓ split into int_parts Vec   ← HEAP ALLOC
  [len][sign][LE bytes] → buf              ↓ DecimalParts { vec, ... }  ← struct + vec
                                           ↓ ColumnValues::Decimal(...) ← enum tag
                                           ↓ match → reassemble i128   ← redundant work
                                           ↓ write_tds_decimal128()
                                           [len][sign][LE bytes] → buf
                                           ↓ Vec dropped               ← DEALLOC
```

### Cost Breakdown

| Overhead Source | Per Row (5 cols) | Impact |
|----------------|-----------------|--------|
| `Vec<ColumnValues>` allocation | 1 heap alloc | Allocator pressure |
| `SqlString` UTF-16 byte Vec | 1 heap alloc + ~40 bytes copied | Dominant cost for string-heavy workloads |
| `DecimalParts.int_parts` Vec | 1 heap alloc + decompose/recompose i128 | Redundant decomposition |
| Enum tag construction | 5 enum writes | Branch predictor noise |
| Enum match dispatch | 5 match branches | Minor but measurable |
| **Total per row** | **3 heap allocs, 5 enum ops** | **~20–48% slower** |

### Why Pre-encoding Strings Helps Further

The `arrow_direct_preencoded` path separates UTF-8 → UTF-16 transcoding from
row iteration. This is effective because:

1. **Batch-level allocation**: One `Vec<Vec<u8>>` for all strings vs. N
   individual allocations
2. **Better cache locality**: String encoding is a separate pass over
   contiguous data
3. **Row loop becomes copy-only**: No transcoding in the hot loop, just
   `memcpy` of pre-encoded bytes

---

## 5. End-to-End SQL Server Benchmark

The serialization-only benchmark (Section 3) isolates CPU cost. To measure
real-world impact including network I/O and SQL Server processing, we ran
end-to-end bulk copy benchmarks against a live SQL Server 2022 instance.

### Changes Required

To enable the direct path, we added **typed write methods** to
`StreamingBulkLoadWriter` that bypass the `ColumnValues` enum entirely:

```rust
// New typed methods — call TdsValueSerializer directly, no enum dispatch
writer.write_int32(col, value).await?;      // Arrow Int32Array → INTN
writer.write_int64(col, value).await?;      // Arrow Int64Array → INTN
writer.write_float64(col, value).await?;    // Arrow Float64Array → FLOATN
writer.write_nvarchar_str(col, &str).await?; // Arrow Utf8Array → NVARCHAR
writer.write_decimal(col, &parts).await?;   // Arrow Decimal128 → DECIMALN
```

We also fixed `serialize_string_utf16` to use bulk `write_async()` instead of
per-code-unit `write_u16_async()` calls, and corrected the NVARCHAR length
prefix to write byte count (not character count).

### Two Paths Benchmarked

```
PATH A: Arrow-Direct (pre-serialized TDS bytes)
════════════════════════════════════════════════

 Arrow column buffers (contiguous memory)
    ↓
 Tight loop per column type:
    Int32Array  → [len=4][i32 LE] per row          ← no function call overhead
    Int64Array  → [len=8][i64 LE] per row
    Float64Array→ [len=8][f64 LE] per row
    Utf8Array   → [byte_count_u16][UTF-16LE] per row ← inline transcode
    Decimal128  → [len=9][sign][8B LE] per row
    ↓
 Pre-serialized TDS bytes per row (Vec<u8>)
    ↓
 BulkLoadRow::write_to_packet()
    ↓
 writer.write_raw_bytes(&tds_bytes)                 ← single bulk write per row
    ↓
 PacketWriter → Network → SQL Server


PATH B: Arrow → Materialized ColumnValues → TDS
═══════════════════════════════════════════════

 Arrow column buffers
    ↓
 Per-row: extract values, allocate intermediates:
    i32        → ColumnValues::Int(i32)              ← enum construction
    i64        → ColumnValues::BigInt(i64)
    f64        → ColumnValues::Float(f64)
    &str       → Vec<u8> UTF-16 alloc → SqlString   ← HEAP ALLOC per row
               → ColumnValues::String(SqlString)
    i128       → Vec<i32> int_parts alloc            ← HEAP ALLOC per row
               → DecimalParts → ColumnValues::Decimal
    ↓
 Vec<ColumnValues> per row                           ← HEAP ALLOC per row
    ↓
 BulkLoadRow::write_to_packet()
    ↓
 writer.write_column_value() × 5
    ↓ serialize_value_inner() → match on enum         ← per-column dispatch
    ↓ serialize_int/float/string/decimal per value     ← per-value async calls
    ↓
 PacketWriter → Network → SQL Server
```

### Results (100K rows, 10 iterations, SQL Server 2022)

| Path | Avg Time | Warm Avg | Throughput | vs. Materialized |
|------|---------|---------|-----------|-----------------|
| **Arrow-direct (pre-serialized)** | 278 ms | 276 ms | **362.8 Krows/s** | **2.09× faster** |
| Materialized ColumnValues | 577 ms | 576 ms | 173.7 Krows/s | baseline |

```
  Throughput (Krows/sec) — end-to-end including network + SQL Server
  ──────────────────────────────────────────────────
  0        100      200      300      400
  ├────────┼────────┼────────┼────────┤

  Materialized (via ColumnValues)
  ██████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░ 174 Krows/s

  Arrow-direct (pre-serialized TDS bytes)
  █████████████████████████████████████████████████░ 363 Krows/s  (+109%)
```

### Key Findings

1. **2.09× end-to-end speedup** — Arrow-direct eliminates per-row heap
   allocations, enum dispatch, and per-value async serializer calls
2. **Columnar processing matters** — iterating each Arrow column in a tight
   loop (all INT values, then all BIGINT values, etc.) is cache-friendly
   and avoids per-value function call overhead from `TdsValueSerializer`
3. **Single bulk write per row** — `write_raw_bytes(&tds_bytes)` does one
   async write per row vs. 5+ async calls in the ColumnValues path
4. **String transcoding is unavoidable** — UTF-8→UTF-16 must happen either way,
   but the direct path avoids the intermediate `Vec<u8>` + `SqlString` wrapper

### Cost Breakdown: What's Eliminated

| Overhead Source | Per Row (Materialized) | Per Row (Arrow-Direct) |
|----------------|----------------------|----------------------|
| `Vec<ColumnValues>` allocation | 1 heap alloc | ✗ eliminated |
| `SqlString` UTF-16 byte Vec | 1 heap alloc + copy | ✗ writes inline |
| `DecimalParts.int_parts` Vec | 1 heap alloc | ✗ writes i128 directly |
| Enum construction (5 cols) | 5 enum tags | ✗ eliminated |
| `serialize_value_inner` match | 5 branches | ✗ eliminated |
| Async write calls per row | 5+ separate calls | 1 bulk write |

### Implications

For a real bulk copy of **1 million rows** with this schema:

| Path | Estimated Time | Savings |
|------|---------------|---------|
| Materialized ColumnValues | ~5.8 s | — |
| Arrow-direct (pre-serialized) | ~2.8 s | **~3.0 s saved** |

---

## 6. Recommended Architecture

```
                    ┌──────────────────────────────────────┐
                    │         Arrow RecordBatch             │
                    │  ┌────────┬────────┬────────┬─────┐  │
                    │  │Int32Arr│Int64Arr│Utf8Arr │Dec  │  │
                    │  │████████│████████│████████│█████│  │
                    │  └───┬────┴───┬────┴───┬────┴──┬──┘  │
                    └──────┼────────┼────────┼───────┼──────┘
                           │        │        │       │
                    ┌──────▼────────▼────────▼───────▼──────┐
                    │   pre_serialize_arrow_to_tds()          │
                    │                                        │
                    │  For each column (tight columnar loop): │
                    │    INT:   [len=4][i32 LE]              │
                    │    NVAR:  [byte_ct][UTF-16LE]          │
                    │    DEC:   [len=9][sign][8B LE]         │
                    │                                        │
                    │  Output: Vec<Vec<u8>> per-row TDS blobs│
                    └──────────────────┬───────────────────┘
                                       │
                    ┌──────────────────▼───────────────────┐
                    │  BulkLoadRow::write_to_packet()        │
                    │                                        │
                    │  writer.write_raw_bytes(&tds_bytes)    │
                    │  — single bulk write per row —         │
                    └──────────────────┬───────────────────┘
                                       │
                    ┌──────────────────▼───────────────────┐
                    │  StreamingBulkLoadWriter / PacketWriter │
                    │                                        │
                    │  [ROW token][pre-serialized bytes]...   │
                    └──────────────────┬───────────────────┘
                                       │
                                       ▼
                                  SQL Server
```

---

## 7. How to Reproduce

```bash
# Serialization-only benchmark (no SQL Server needed)
cargo bench --bench arrow_tds_bench

# End-to-end SQL Server benchmark (requires live instance)
DB_HOST=localhost DB_PORT=1433 DB_USERNAME=sa TRUST_SERVER_CERTIFICATE=true \
  cargo nextest run -p mssql-tds --test bench_arrow_bulk_copy --run-ignored all \
  --success-output immediate

# View Criterion HTML report
open target/criterion/arrow_to_tds_bulk_copy/report/index.html
```

### Benchmark Sources

- `mssql-tds/benches/arrow_tds_bench.rs` — serialization-only (Criterion)
- `mssql-tds/tests/bench_arrow_bulk_copy.rs` — end-to-end SQL Server

---

## 8. Conclusion

The intermediate `ColumnValues` representation imposes a **2.09× end-to-end
penalty** for bulk copy throughput against a real SQL Server. The dominant costs
are **per-row heap allocations** (Vec<ColumnValues>, SqlString, DecimalParts),
**per-value async serializer calls**, and **enum dispatch overhead**.

The pre-serialization approach — walking Arrow column buffers in tight loops to
produce TDS wire bytes synchronously, then writing them in a single
`write_raw_bytes` call per row — eliminates all of these overheads.

### Changes Made

1. `pre_serialize_arrow_to_tds()` — synchronous Arrow→TDS pre-serializer that
   iterates each column buffer in a tight loop, producing one `Vec<u8>` per row
   containing all TDS wire bytes ready for `write_raw_bytes`
2. `write_raw_bytes()` on `StreamingBulkLoadWriter` — single bulk write of
   pre-serialized TDS bytes, bypassing all TdsValueSerializer logic
3. Made key serializer methods `pub(crate)` for the intermediate typed-write
   approach used for comparison
4. Optimized `serialize_string_utf16` to use bulk `write_async()` instead of
   per-code-unit writes, and fixed the NVARCHAR length prefix bug (byte count,
   not character count — SQL Server error 4895)

### Next Steps

1. Integrate `pre_serialize_arrow_to_tds` into the product code as a first-class
   `ArrowBulkLoadRow` implementation
2. Extend pre-serialization for remaining types (Date, Time, DateTime2, UUID,
   Binary, Bit)
3. Explore batch-level pre-serialization (multiple rows per `write_raw_bytes`
   call) for further amortization of async overhead
4. Profile memory allocation patterns to quantify GC-pressure reduction in
   FFI scenarios (Python/Node.js callers)

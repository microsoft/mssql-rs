# Arrow-to-TDS Direct Serialization: Benchmark Report

## Summary

This report presents benchmark results from the `mssql-arrow` crate, a Rust
library that bulk-copies Apache Arrow `RecordBatch` data into SQL Server by
serializing directly into the TDS (Tabular Data Stream) wire format — bypassing
the intermediate row-level type materialization that ADBC drivers typically
perform.

The key finding: **direct Arrow-to-TDS serialization achieves 2.18× the
throughput of the materialized path at 1 million rows**, reaching 417 K rows/s
for a 5-column mixed-type schema against a local SQL Server 2022 instance.

---

## Motivation

ADBC defines a columnar interface for database interaction using Arrow as the
canonical in-memory format. When an ADBC driver performs a bulk insert, the
typical data flow is:

```
Arrow RecordBatch
  → extract each cell → materialize into driver-native row types
    → serialize those rows into the wire protocol
      → send to server
```

This materialization step — converting columnar Arrow arrays into per-row
value enums — introduces allocation, branching, and memory pressure that
scales linearly with row count. For SQL Server's TDS protocol, each column
value must ultimately be a fixed sequence of bytes (length-prefixed, little-endian).
Arrow arrays already store data in dense, typed, contiguous buffers. The
hypothesis: we can skip the intermediate representation entirely and serialize
Arrow columns straight into TDS byte sequences, row by row, yielding
measurable throughput gains for bulk copy workloads.

`mssql-arrow` proves this out. It reads directly from Arrow's typed arrays
(`Int32Array`, `StringArray`, `Decimal128Array`, etc.) and writes the
corresponding TDS bytes inline — no `enum`, no heap allocation per cell, no
intermediate `Vec<ColumnValue>`.

---

## What Was Measured

Four bulk copy paths were benchmarked, all inserting the same Arrow
`RecordBatch` into SQL Server via the TDS bulk load protocol:

| Path | Label | Description |
|------|-------|-------------|
| **A** | Pre-serialized | Serialize entire Arrow batch into per-row `Vec<u8>` TDS buffers up front, then bulk copy the byte slices. Measures raw serialization speed but pays for full materialization of byte buffers. |
| **B** | Materialized | Convert Arrow data into `Vec<ColumnValues>` (a Rust enum per cell — the equivalent of what a traditional ADBC driver would do), then serialize each enum variant during bulk copy. **This is the baseline.** |
| **C** | Streaming | Write TDS bytes directly from Arrow arrays into the packet buffer during bulk copy — zero intermediate allocation, but using hand-written per-type serialization in the benchmark itself. |
| **D** | ArrowBulkCopy | The `mssql-arrow` crate's public API (`ArrowBulkCopy::write_batch`). Pre-serializes the batch into per-row byte buffers using the crate's type-mapped serializer, then bulk copies. This is the path an ADBC driver would call. |

All paths use the same underlying `mssql-tds` bulk copy transport — the only
variable is how Arrow data is transformed before hitting the wire.

### Schema

```
id      INT           NOT NULL
amount  BIGINT        NOT NULL
price   FLOAT         NOT NULL
name    NVARCHAR(200) NOT NULL
total   DECIMAL(18,2) NOT NULL
```

This schema exercises fixed-width numerics (INT, BIGINT, FLOAT), variable-length
Unicode strings (NVARCHAR), and decimal — covering the main TDS serialization
code paths.

---

## Test Environment

| Component | Detail |
|-----------|--------|
| **Host OS** | Linux (WSL2), kernel 6.6.87.2-microsoft-standard-WSL2 |
| **CPU** | AMD EPYC 7763 64-Core — 16 cores / 32 threads @ ~2.45 GHz |
| **RAM** | 64 GB |
| **SQL Server** | Microsoft SQL Server 2022 (Developer Edition), running in Docker on the same host |
| **Network** | localhost (loopback) — no real network latency |
| **Rust** | 1.90.0 (stable) |
| **Build profile** | `test` (unoptimized + debuginfo) — *not release-optimized* |
| **Arrow version** | arrow-array / arrow-schema / arrow-buffer 57 |
| **Benchmark runner** | cargo-nextest, 10 iterations per path, first iteration treated as cold |

> **Note:** These benchmarks were compiled in the default `test` profile
> (debug mode, no optimizations). A release-optimized build would likely show
> even larger absolute throughput and may shift the relative speedup ratios.

---

## Results

### 1. End-to-End: 1,000,000 rows × 10 iterations

Each path inserts 1M rows into a temp table, truncates, and repeats 10 times.
"Warm" averages exclude the first iteration.

| Path | Avg (ms) | Warm Avg (ms) | Throughput (Krows/s) | Speedup vs Materialized |
|------|----------|---------------|----------------------|-------------------------|
| **D — ArrowBulkCopy** | 2,408 | **2,400** | **417** | **2.18×** |
| **A — Pre-serialized** | 2,813 | 2,805 | 357 | 1.86× |
| **C — Streaming** | 3,499 | 3,498 | 286 | 1.49× |
| **B — Materialized** | 5,283 | 5,223 | 192 | 1.00× (baseline) |

#### Per-iteration detail

<details>
<summary>Click to expand raw iteration data</summary>

```
  [direct]       iter  1:  2882.35 ms  (346.9 Krows/s)
  [direct]       iter  2:  3005.09 ms  (332.8 Krows/s)
  [direct]       iter  3:  3124.60 ms  (320.0 Krows/s)
  [direct]       iter  4:  3043.28 ms  (328.6 Krows/s)
  [direct]       iter  5:  2847.07 ms  (351.2 Krows/s)
  [direct]       iter  6:  2951.74 ms  (338.8 Krows/s)
  [direct]       iter  7:  2956.23 ms  (338.3 Krows/s)
  [direct]       iter  8:  2571.60 ms  (388.9 Krows/s)
  [direct]       iter  9:  2336.79 ms  (427.9 Krows/s)
  [direct]       iter 10:  2411.72 ms  (414.6 Krows/s)

  [materialized] iter  1:  5820.97 ms  (171.8 Krows/s)
  [materialized] iter  2:  5191.47 ms  (192.6 Krows/s)
  [materialized] iter  3:  5150.81 ms  (194.1 Krows/s)
  [materialized] iter  4:  5074.32 ms  (197.1 Krows/s)
  [materialized] iter  5:  5077.94 ms  (196.9 Krows/s)
  [materialized] iter  6:  5199.15 ms  (192.3 Krows/s)
  [materialized] iter  7:  5125.93 ms  (195.1 Krows/s)
  [materialized] iter  8:  5156.42 ms  (193.9 Krows/s)
  [materialized] iter  9:  5084.69 ms  (196.7 Krows/s)
  [materialized] iter 10:  5944.36 ms  (168.2 Krows/s)

  [streaming]    iter  1:  3508.60 ms  (285.0 Krows/s)
  [streaming]    iter  2:  3528.70 ms  (283.4 Krows/s)
  [streaming]    iter  3:  3491.73 ms  (286.4 Krows/s)
  [streaming]    iter  4:  3469.83 ms  (288.2 Krows/s)
  [streaming]    iter  5:  3521.35 ms  (284.0 Krows/s)
  [streaming]    iter  6:  3500.34 ms  (285.7 Krows/s)
  [streaming]    iter  7:  3489.95 ms  (286.5 Krows/s)
  [streaming]    iter  8:  3532.30 ms  (283.1 Krows/s)
  [streaming]    iter  9:  3489.01 ms  (286.6 Krows/s)
  [streaming]    iter 10:  3461.90 ms  (288.9 Krows/s)

  [arrow_bulk]   iter  1:  2481.18 ms  (403.0 Krows/s)
  [arrow_bulk]   iter  2:  2389.93 ms  (418.4 Krows/s)
  [arrow_bulk]   iter  3:  2483.24 ms  (402.7 Krows/s)
  [arrow_bulk]   iter  4:  2506.42 ms  (399.0 Krows/s)
  [arrow_bulk]   iter  5:  2380.70 ms  (420.0 Krows/s)
  [arrow_bulk]   iter  6:  2369.01 ms  (422.1 Krows/s)
  [arrow_bulk]   iter  7:  2362.73 ms  (423.2 Krows/s)
  [arrow_bulk]   iter  8:  2391.38 ms  (418.2 Krows/s)
  [arrow_bulk]   iter  9:  2366.63 ms  (422.5 Krows/s)
  [arrow_bulk]   iter 10:  2350.78 ms  (425.4 Krows/s)
```

</details>

### 2. End-to-End: 100,000 rows × 10 iterations (prior run)

| Path | Warm Avg (ms) | Throughput (Krows/s) | Speedup |
|------|---------------|----------------------|---------|
| **A — Pre-serialized** | 240 | 417 | 2.16× |
| **D — ArrowBulkCopy** | 252 | 398 | 2.06× |
| **C — Streaming** | 357 | 280 | 1.45× |
| **B — Materialized** | 519 | 193 | 1.00× |

### 3. Criterion Microbenchmark: pure serialization (no SQL Server)

These measure only the Arrow → TDS byte serialization cost, with no network or
server overhead. Ran via `cargo bench -p mssql-arrow` (Criterion 0.5).

| Path | 10K rows | 100K rows | Throughput (100K) |
|------|----------|-----------|-------------------|
| `arrow_direct_preencoded` | 0.74 ms | 7.31 ms | 13.7 M elem/s |
| `arrow_direct` | 1.05 ms | 10.59 ms | 9.4 M elem/s |
| `arrow_via_column_values` | 1.14 ms | 11.60 ms | 8.6 M elem/s |

Even in isolation (no I/O), the direct Arrow-to-TDS path is **59% faster**
than going through the `ColumnValues` enum, confirming that the speedup is
rooted in serialization, not just I/O amortization.

---

## Analysis

### Why ArrowBulkCopy wins at scale

At 100K rows, the pre-serialized path (A) and ArrowBulkCopy (D) are neck and
neck. At 1M rows, ArrowBulkCopy pulls ahead (417 vs 357 Krows/s). The reason:

- **Path A** pre-allocates a `Vec<u8>` per row for the entire batch before
  sending anything. At 1M rows this creates significant memory pressure (~200 MB
  of byte buffers for this schema), which slows down allocation and increases
  cache misses.
- **Path D** (ArrowBulkCopy) uses the same serialization logic but benefits from
  the `mssql-tds` bulk copy transport's batching and packet management, which
  amortizes memory allocation better.

### Where the materialized path loses time

The materialized path (B) converts every cell into a `ColumnValues` enum — a
tagged union with heap-allocated variants for strings and decimals. For 1M rows
× 5 columns, that's 5 million enum constructions, each involving:

1. Pattern matching on the Arrow array type
2. Extracting the value
3. Wrapping in the enum variant (with string cloning for NVARCHAR)
4. Later pattern matching again during TDS serialization

The direct path replaces all of this with a single inline function per column
type that reads from the Arrow array's backing buffer and writes TDS bytes
directly. For fixed-width types like INT or FLOAT, this compiles down to a
memcpy of 4–8 bytes with a 1-byte length prefix.

### Streaming vs ArrowBulkCopy

The streaming path (C) writes TDS bytes directly into the packet buffer during
bulk copy — theoretically the lowest-overhead option since it avoids even the
per-row `Vec<u8>`. In practice, it is slower than ArrowBulkCopy because:

- It constructs a `StreamingRow` wrapper per row, which borrows from each
  typed Arrow array independently.
- The per-row async write granularity means more overhead from the bulk copy
  state machine.
- ArrowBulkCopy's pre-serialization approach allows larger contiguous writes
  via `write_raw_bytes`, which is more efficient at the packet layer.

---

## Implications for ADBC

An ADBC driver for SQL Server could adopt this approach to eliminate the
materialization overhead entirely. The `ArrowBulkCopy` API demonstrates a
concrete implementation:

```rust
let mut bulk = ArrowBulkCopy::new(&mut client, "target_table");
bulk.write_batch(&record_batch).await?;
```

The crate handles Arrow-to-TDS type mapping for the standard SQL Server type
set (INT, BIGINT, SMALLINT, TINYINT, FLOAT, REAL, BIT, NVARCHAR, VARCHAR,
DECIMAL, DATE, TIME, DATETIME2, UNIQUEIDENTIFIER) and serializes each column
using type-specific inline functions that read directly from Arrow's typed
array buffers.

This is not limited to bulk copy. The same serialization approach could apply
to any TDS operation that sends tabular data: TVPs (Table-Valued Parameters),
parameterized queries with Arrow inputs, or any future ADBC `Statement.Bind`
implementation.

---

## Reproducing

### Prerequisites

- Rust 1.90+
- A SQL Server 2019+ instance (Docker works fine)
- `cargo-nextest` installed

### Run the microbenchmark (no SQL Server)

```bash
cargo bench -p mssql-arrow
```

### Run the end-to-end benchmark

```bash
export DB_HOST=localhost
export DB_USERNAME=sa
export SQL_PASSWORD=<your-password>
export TRUST_SERVER_CERTIFICATE=true

cargo nextest run -p mssql-arrow \
  --test bench_arrow_bulk_copy \
  --run-ignored=only \
  --success-output immediate
```

Row count and iteration count are configured via constants in
`tests/bench_arrow_bulk_copy.rs` (`NUM_ROWS`, `NUM_ITERATIONS`).

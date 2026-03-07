# mssql-arrow

High-performance Arrow RecordBatch bulk copy for SQL Server via `mssql-tds`.

## Crate Overview

`mssql-arrow` provides a zero-copy serialization path from Apache Arrow `RecordBatch` data directly into TDS wire format for SQL Server bulk insert operations. The primary API surface:

- **`ArrowBulkCopy`** — Builder for bulk inserting Arrow RecordBatch data into SQL Server tables.
- **`ArrowQueryReader`** — Reads SQL Server query results into Arrow RecordBatch format.
- **`ArrowError`** — Error type for Arrow-specific failures.

## Benchmarks

The crate includes two benchmark suites that measure different aspects of Arrow-to-TDS serialization performance.

### 1. Criterion Microbenchmark (no SQL Server required)

Measures pure serialization throughput — Arrow RecordBatch → TDS wire bytes — with no network I/O. Compares three serialization strategies:

| Path | Description |
|------|-------------|
| `arrow_direct` | Read typed values from Arrow arrays, write TDS bytes directly |
| `arrow_direct_preencoded` | Same as direct, but pre-encodes all strings to UTF-16 at batch level |
| `arrow_via_column_values` | Convert Arrow values to `ColumnValues` enum first, then serialize to TDS |

Each path is benchmarked at two dataset sizes: **10,000** and **100,000** rows.

**Schema:** `INT, BIGINT, FLOAT, NVARCHAR, DECIMAL(18,2)`

#### Run

```bash
cargo bench -p mssql-arrow
```

Results are written to `target/criterion/` with HTML reports.

To run a specific benchmark variant:

```bash
cargo bench -p mssql-arrow -- arrow_direct/10000
```

### 2. End-to-End SQL Server Benchmark (requires live SQL Server)

Measures real bulk copy throughput against a SQL Server instance, including network I/O and server-side insert cost. Compares four bulk copy approaches:

| Path | Description |
|------|-------------|
| **A** — Pre-serialized | Pre-serialize Arrow columns into per-row TDS byte buffers, then bulk copy via `write_raw_bytes` |
| **B** — Materialized | Convert Arrow data to `Vec<ColumnValues>` rows, bulk copy via `write_column_value` |
| **C** — Streaming | Write TDS bytes directly to packet buffer from Arrow arrays — zero intermediate allocation |
| **D** — ArrowBulkCopy | Use the `mssql-arrow` crate's high-level `ArrowBulkCopy` API |

Runs **100,000 rows × 10 iterations** per path, reports per-iteration throughput (Krows/s), and prints a summary with average/warm speedup ratios relative to the materialized baseline.

**Schema:** `INT, BIGINT, FLOAT, NVARCHAR(200), DECIMAL(18,2)`

#### Prerequisites

Set connection environment variables (or use a `.env` file in the workspace root):

```bash
export DB_HOST=localhost
export DB_USERNAME=sa
export SQL_PASSWORD=<your-password>
export TRUST_SERVER_CERTIFICATE=true
# Optional:
export DB_PORT=1433
```

#### Run

```bash
cargo nextest run -p mssql-arrow --test bench_arrow_bulk_copy --run-ignored=only
```

The test is marked `#[ignore]` so it won't run during normal `cargo btest`. You must explicitly opt in with `--run-ignored=only`.

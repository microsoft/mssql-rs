# Quickstart: Arrow Benchmark Suite

**Feature**: 003-move-arrow-benchmarks

## Running Local Criterion Benchmarks

No SQL Server required. Measures serialization throughput for three Arrow→TDS conversion strategies.

```bash
# Run all Criterion benchmarks in mssql-arrow
cargo bench -p mssql-arrow

# Run a specific benchmark
cargo bench -p mssql-arrow -- arrow_direct

# Generate HTML report (opens in target/criterion/report/index.html)
cargo bench -p mssql-arrow
```

Output includes throughput in elements/sec at 10K and 100K row sizes for:
- `arrow_direct` — Arrow arrays → TDS bytes directly
- `arrow_direct_preencoded` — same, with batch-level UTF-16 pre-encoding
- `arrow_via_column_values` — Arrow → ColumnValues enum → TDS bytes

## Running End-to-End SQL Server Benchmarks

Requires a live SQL Server instance. Tests real bulk copy throughput.

### Prerequisites

1. Create or update `.env` at the workspace root:
```env
DB_HOST=localhost
DB_PORT=1433
DB_USERNAME=sa
SQL_PASSWORD=YourPassword123
TRUST_SERVER_CERTIFICATE=true
```

2. Verify SQL Server is accessible.

### Run

```bash
# Run the ignored benchmark test
cargo nextest run -p mssql-arrow --run-ignored=only bench_arrow_bulk_copy

# With trace logging enabled
ENABLE_TEST_TRACE=true cargo nextest run -p mssql-arrow --run-ignored=only bench_arrow_bulk_copy
```

### Expected Output

```
============================================================
Arrow Bulk Copy Benchmark — 100000 rows × 10 iterations
Schema: INT, BIGINT, FLOAT, NVARCHAR(200), DECIMAL(18,2)
Paths: A=pre-serialized, B=materialized, C=streaming, D=ArrowBulkCopy
============================================================

  [direct]       iter 1:   275.34 ms  (363.2 Krows/s)
  [direct]       iter 2:   268.11 ms  (372.9 Krows/s)
  ...
  [materialized] iter 1:   574.12 ms  (174.2 Krows/s)
  ...
  [streaming]    iter 1:   245.67 ms  (407.1 Krows/s)
  ...
  [arrow_bulk_copy] iter 1:  260.89 ms (383.3 Krows/s)
  ...

------------------------------------------------------------
RESULTS (100000 rows, 10 iterations)
------------------------------------------------------------
  Pre-serialized (avg):      271.23 ms  (368.7 Krows/s)
  Streaming (avg):           248.45 ms  (402.5 Krows/s)
  ArrowBulkCopy (avg):       258.12 ms  (387.4 Krows/s)
  Materialized (avg):        569.78 ms  (175.5 Krows/s)
  ...
------------------------------------------------------------
```

## Interpreting Results

- **Speedup ratios** compare each path against the materialized baseline (Path B), which goes through the `ColumnValues` intermediate representation
- **Warm averages** exclude the first iteration to remove cold-start effects
- **Throughput** is reported in Krows/s (thousands of rows per second)
- The ArrowBulkCopy path should be within 80% of the best manual path

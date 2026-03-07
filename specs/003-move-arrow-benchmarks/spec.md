# Feature Specification: Move Arrow Benchmarks to mssql-arrow Crate

**Feature Branch**: `003-move-arrow-benchmarks`  
**Created**: 2025-07-15  
**Status**: Draft  
**Input**: User description: "Move Arrow benchmarks comparing bulk copy approaches to the new mssql-arrow crate"

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Run Criterion Microbenchmarks Locally (Priority: P1)

A developer working on Arrow-to-TDS serialization performance runs the local Criterion benchmark suite from the `mssql-arrow` crate to measure serialization throughput without needing a live SQL Server. The benchmark compares three serialization paths: Arrow-direct, Arrow-direct with pre-encoded strings, and Arrow-via-intermediate-representation. Results are reported in rows/second at multiple dataset sizes.

**Why this priority**: Local benchmarks are the primary tool for iterating on performance. They must work without external dependencies and provide immediate, reproducible feedback on serialization throughput regressions.

**Independent Test**: Run `cargo bench -p mssql-arrow` and observe Criterion output with throughput statistics for all three serialization paths at 10K and 100K row sizes.

**Acceptance Scenarios**:

1. **Given** the mssql-arrow crate with benchmark files in place, **When** a developer runs `cargo bench -p mssql-arrow`, **Then** Criterion outputs throughput measurements (elements/sec) for all three serialization paths at both 10,000 and 100,000 row dataset sizes.
2. **Given** a Criterion benchmark run, **When** results are produced, **Then** each benchmark reports mean time, standard deviation, and throughput in a format compatible with Criterion's HTML report generation.
3. **Given** the benchmark suite, **When** run on a machine without SQL Server, **Then** all benchmarks complete without network connections or external dependencies.

---

### User Story 2 - Run End-to-End SQL Server Benchmark (Priority: P2)

A developer with access to a live SQL Server instance runs the end-to-end benchmark from `mssql-arrow` to measure real bulk copy throughput across three approaches: pre-serialized TDS bytes, materialized intermediate values, and streaming direct-to-packet writes. The benchmark reports throughput in Krows/s, compares warm vs cold iterations, and verifies data integrity via row count assertions.

**Why this priority**: End-to-end benchmarks validate that serialization performance translates to actual SQL Server bulk copy throughput, including network and server-side costs. They are gated behind `#[ignore]` and require a `.env` with connection details.

**Independent Test**: With a SQL Server instance configured via `.env`, run `cargo nextest run -p mssql-arrow --run-ignored=only bench_arrow_bulk_copy` and observe per-iteration throughput for all three paths plus a summary with speedup ratios.

**Acceptance Scenarios**:

1. **Given** a configured `.env` file with SQL Server connection details, **When** a developer runs the ignored benchmark test, **Then** the test creates temporary tables, inserts 100,000 rows per path across 10 iterations, and reports per-iteration and average throughput.
2. **Given** a completed benchmark run, **When** results are printed, **Then** the summary includes warm (skip first iteration) and cold averages, throughput in Krows/s, and speedup ratios for pre-serialized and streaming paths relative to the materialized baseline.
3. **Given** all three bulk copy paths complete, **When** row counts are verified, **Then** each temporary table contains exactly the expected number of rows.

---

### User Story 3 - Add ArrowBulkCopy Path to End-to-End Benchmark (Priority: P3)

The end-to-end benchmark includes a fourth comparison path that uses the `mssql-arrow` crate's own `ArrowBulkCopy` API. This demonstrates the crate's value by benchmarking the high-level API alongside the hand-tuned paths, showing that the crate achieves comparable or better throughput with a simpler API surface.

**Why this priority**: Validates that the mssql-arrow crate's public API delivers performance competitive with hand-optimized approaches. This is a showcase for the crate's purpose but depends on the existing three paths being migrated first.

**Independent Test**: Run the end-to-end benchmark and compare the ArrowBulkCopy path's throughput against the three existing paths.

**Acceptance Scenarios**:

1. **Given** the end-to-end benchmark with all four paths, **When** the benchmark runs, **Then** the ArrowBulkCopy path reports throughput alongside the three manual paths.
2. **Given** benchmark results for all four paths, **When** comparing throughput, **Then** the ArrowBulkCopy path achieves at least 80% of the throughput of the best manual path (pre-serialized or streaming).

---

### Edge Cases

- What happens when the Criterion benchmark is run with very small row counts (e.g., 1 or 10)? Benchmark should still produce valid timing data without division-by-zero or measurement noise issues.
- What happens when the end-to-end benchmark is run without a valid `.env` file? The test should fail with a clear connection error rather than a panic.
- How does the benchmark handle SQL Server temporary table collisions if a previous run was interrupted? Benchmark uses `CREATE TABLE` (not `IF NOT EXISTS`), so a stale temp table from a crashed session should not exist (temp tables are session-scoped).
- What happens when the benchmark is run on a slow network? All paths should still complete but throughput numbers will be lower — the relative speedup ratios remain the meaningful comparison.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The Criterion microbenchmark MUST be located in the `mssql-arrow` crate's bench directory and runnable via `cargo bench -p mssql-arrow`.
- **FR-002**: The Criterion benchmark MUST compare three serialization paths: Arrow-direct, Arrow-direct with pre-encoded UTF-16 strings, and Arrow-via-intermediate-representation.
- **FR-003**: The Criterion benchmark MUST test at two dataset sizes (10,000 and 100,000 rows) with throughput reported in elements/second.
- **FR-004**: The end-to-end benchmark MUST be located in the `mssql-arrow` crate's test directory and marked `#[ignore]` to avoid running in CI without a SQL Server instance.
- **FR-005**: The end-to-end benchmark MUST compare at least three bulk copy approaches: pre-serialized TDS bytes (Path A), materialized intermediate values (Path B), and streaming direct-to-packet writes (Path C).
- **FR-006**: The end-to-end benchmark MUST report per-iteration timing, average throughput in Krows/s, warm-up-excluded averages, and speedup ratios relative to the materialized baseline.
- **FR-007**: The end-to-end benchmark MUST verify data integrity by asserting row counts match the expected number of inserted rows for each path.
- **FR-008**: Both benchmarks MUST use a consistent test schema: 5 columns (integer, big integer, floating-point, variable-length string, decimal with precision 18 and scale 2).
- **FR-009**: The benchmark files MUST be removed from the `mssql-tds` crate after migration to avoid duplication.
- **FR-010**: The end-to-end benchmark SHOULD include a fourth path using the `mssql-arrow` crate's `ArrowBulkCopy` API for comparison.
- **FR-011**: The Criterion benchmark MUST NOT require any external services (SQL Server, network) to run.
- **FR-012**: The `mssql-arrow` crate's build configuration MUST declare any additional dependencies required by the benchmarks (e.g., Criterion as a dev-dependency, bench harness configuration).

### Key Entities

- **Serialization Path**: A distinct approach to converting Arrow columnar data into TDS wire-format row tokens. Each path represents a different trade-off between allocation overhead, intermediate representations, and throughput.
- **RecordBatch**: The input dataset — a columnar data structure with a fixed 5-column schema used consistently across all benchmark paths.
- **Benchmark Result**: Per-iteration timing data including elapsed time, throughput (rows/second), warm-up exclusion, and relative speedup ratios.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: All Criterion benchmarks run successfully via `cargo bench -p mssql-arrow` and produce throughput measurements for 6 benchmark variants (3 paths × 2 dataset sizes).
- **SC-002**: The end-to-end benchmark, when run against a SQL Server instance, inserts 100,000 rows per path per iteration and reports throughput above 100 Krows/s for at least one path.
- **SC-003**: No benchmark-related files remain in the `mssql-tds` crate after migration (zero duplication).
- **SC-004**: The benchmark suite adds no new non-dev dependencies to the `mssql-arrow` crate.
- **SC-005**: A developer can run the local Criterion benchmarks in under 2 minutes on standard hardware without any external service configuration.
- **SC-006**: The end-to-end benchmark summary clearly identifies which serialization path achieves the highest throughput, enabling data-driven optimization decisions.

## Assumptions

- The `mssql-arrow` crate already exists with its core API (ArrowBulkCopy, serializer, type_mapping) fully implemented.
- The existing mssql-tds test helper module (`common`) pattern is replicated in mssql-arrow's test infrastructure.
- The `.env` file convention for SQL Server connection details is shared across all crates in the workspace.
- Criterion is the standard benchmarking framework for Rust and is appropriate for this crate.
- The 5-column schema (INT, BIGINT, FLOAT, NVARCHAR(200), DECIMAL(18,2)) is representative of real-world bulk copy workloads for benchmarking purposes.

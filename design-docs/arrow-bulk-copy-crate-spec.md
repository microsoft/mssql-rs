# Feature Specification: `mssql-arrow` Crate — Arrow Bulk Copy for SQL Server

**Feature Branch**: `dev/saurabh/arrow-bulk-copy-crate`  
**Created**: 2026-03-07  
**Status**: Draft  
**Input**: User description: "Develop a separate crate with Arrow bulk copy capability that calls into mssql-tds"

## Context & Motivation

The `mssql-tds` crate implements the TDS protocol and provides a streaming bulk copy API (`BulkCopy`, `StreamingBulkLoadWriter`, `BulkLoadRow`). Experimental work on this branch has demonstrated a **2.09× throughput improvement** (363 vs 174 Krows/s) when serializing Arrow `RecordBatch` data directly to TDS wire bytes, bypassing the intermediate `ColumnValues` enum representation.

This capability currently lives in test/benchmark files (`tests/bench_arrow_bulk_copy.rs`, `benches/arrow_tds_bench.rs`) and a `#[cfg(test)]`-gated `ArrowRowWriter` in `mssql-tds`. The goal is to extract and productionize this into a standalone `mssql-arrow` crate that:

1. Provides a first-class API for bulk-copying Arrow `RecordBatch` data into SQL Server
2. Provides a first-class API for reading SQL Server query results directly into Arrow `RecordBatch` format
3. Lives as a separate crate to keep Arrow dependencies out of the core `mssql-tds` crate
4. Reuses `mssql-tds` public APIs without requiring internal changes to the core crate

### What Exists Today (in `mssql-tds`)

| Component | Location | Visibility | Purpose |
|---|---|---|---|
| `BulkCopy` builder | `connection::bulk_copy` | `pub` | Configures and executes bulk copy operations |
| `BulkLoadRow` trait | `connection::bulk_copy` | `pub` | Trait for rows to serialize themselves to TDS packets |
| `StreamingBulkLoadWriter` | `message::bulk_load` | `pub` | Low-level TDS packet writer with typed write methods |
| `RowWriter` trait | `datatypes::row_writer` | `pub` | Pluggable decode sink for query results (Arrow, NAPI, etc.) |
| `ResultSet::next_row_into()` | `connection::tds_client` | `pub` | Decodes rows directly into a `RowWriter` implementation |
| `ArrowRowWriter` | `datatypes::arrow_writer` | `#[cfg(test)]` | Test-only Arrow RowWriter implementation |
| `pre_serialize_arrow_to_tds()` | `tests/bench_arrow_bulk_copy.rs` | test-only | Proof-of-concept Arrow→TDS pre-serializer |

### Key mssql-tds Public API Surface

The new crate interacts with `mssql-tds` through these public interfaces:

**Bulk Copy (write path)**:
- `BulkCopy::new(&mut TdsClient, table_name)` — creates bulk copy builder
- `.batch_size()`, `.timeout()`, `.check_constraints()`, `.fire_triggers()`, `.keep_identity()`, `.keep_nulls()`, `.table_lock()` — configuration
- `.write_to_server_zerocopy(&[impl BulkLoadRow])` — executes bulk insert
- `BulkLoadRow::write_to_packet(&self, &mut StreamingBulkLoadWriter, &mut usize)` — per-row serialization
- `StreamingBulkLoadWriter::write_raw_bytes(&[u8])` — single bulk write of pre-serialized TDS bytes
- `StreamingBulkLoadWriter::write_int32()`, `write_int64()`, `write_float64()`, `write_nvarchar_str()`, `write_decimal()` — typed direct writes
- `StreamingBulkLoadWriter::write_*_unchecked()`, `has_space()`, `flush_if_needed()` — zero-check buffer writes

**Query Results (read path)**:
- `ResultSet::next_row_into(&mut dyn RowWriter)` — decodes row into a RowWriter
- `ResultSet::get_metadata()` — column metadata for schema inference
- `RowWriter` trait — 25 typed methods (write_null, write_bool, write_i32, write_string, write_decimal, write_datetime2, etc.)

---

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Bulk Copy Arrow RecordBatch to SQL Server (Priority: P1)

A data engineer has an Arrow `RecordBatch` (from Polars, DuckDB, Pandas, or any Arrow-producing source) and wants to bulk-insert it into a SQL Server table with maximum throughput and zero intermediate allocations.

```rust
use mssql_arrow::ArrowBulkCopy;
use mssql_tds::connection::tds_client::TdsClient;
use arrow_array::RecordBatch;

let batch: RecordBatch = /* from Parquet, DataFrame, etc. */;

let result = ArrowBulkCopy::new(&mut client, "dbo.MyTable")
    .batch_size(100_000)
    .table_lock(true)
    .write_record_batch(&batch)
    .await?;

println!("Inserted {} rows in {:.2}s", result.rows_affected, result.elapsed.as_secs_f64());
```

**Why this priority**: This is the core value proposition. The 2.09× speedup demonstrated in benchmarks directly addresses the most common Arrow→SQL Server use case.

**Independent Test**: Can be tested end-to-end with a `RecordBatch` containing INT, BIGINT, FLOAT, NVARCHAR, and DECIMAL columns against a live or mock SQL Server, verifying row count and data correctness.

**Acceptance Scenarios**:

1. **Given** a `RecordBatch` with 100K rows and 5 columns (INT, BIGINT, FLOAT, NVARCHAR(200), DECIMAL(18,2)), **When** `write_record_batch()` is called, **Then** all rows are inserted into the destination table with correct values and the operation completes with ≥1.5× throughput vs the ColumnValues path.

2. **Given** a `RecordBatch` with nullable columns containing null values, **When** `write_record_batch()` is called, **Then** NULLs are correctly represented in the destination table.

3. **Given** a `RecordBatch` with columns that don't match the destination table schema order, **When** column mappings are provided, **Then** columns are mapped correctly by name or ordinal.

4. **Given** an empty `RecordBatch` (0 rows), **When** `write_record_batch()` is called, **Then** the operation succeeds with `rows_affected = 0` and no error.

---

### User Story 2 — Read SQL Server Query Results into Arrow RecordBatch (Priority: P1)

A data engineer wants to execute a query against SQL Server and receive results directly as an Arrow `RecordBatch`, avoiding per-row `ColumnValues` allocations during decoding.

```rust
use mssql_arrow::ArrowQueryReader;
use mssql_tds::connection::tds_client::TdsClient;

client.execute("SELECT id, name, price FROM Products".into(), None, None).await?;

let reader = ArrowQueryReader::new(&mut client);
while let Some(batch) = reader.next_batch(10_000).await? {
    // batch is a RecordBatch with up to 10K rows
    process_batch(&batch);
}
```

**Why this priority**: Reading results into Arrow is equally important for analytics workloads. The `RowWriter` trait and `ArrowRowWriter` already exist as test-only code — productionizing this completes the round-trip.

**Independent Test**: Execute a query returning known data, collect results via `ArrowQueryReader`, verify the `RecordBatch` schema and values match.

**Acceptance Scenarios**:

1. **Given** a query returning rows with INT, BIGINT, FLOAT, NVARCHAR, DECIMAL, DATE, DATETIME2, and UNIQUEIDENTIFIER columns, **When** `next_batch()` is called, **Then** the returned `RecordBatch` has correct Arrow types and values for each column.

2. **Given** a query returning 250K rows with `batch_size = 10_000`, **When** iteration completes, **Then** exactly 25 batches are returned, each with 10K rows, and all rows are accounted for.

3. **Given** a query returning columns with NULL values, **When** results are read, **Then** Arrow null bitmaps are correctly populated.

4. **Given** a query returning multiple result sets, **When** the reader is used, **Then** each result set is independently consumable as batches.

---

### User Story 3 — Stream Multiple RecordBatches (Priority: P2)

A data pipeline produces Arrow `RecordBatch` values incrementally (e.g., reading a large Parquet file in chunks) and wants to stream them into SQL Server without accumulating the entire dataset in memory.

```rust
let mut bulk = ArrowBulkCopy::new(&mut client, "dbo.BigTable")
    .batch_size(50_000)
    .table_lock(true);

for batch in parquet_reader.batches() {
    bulk.write_record_batch(&batch).await?;
}

let result = bulk.finish().await?;
```

**Why this priority**: Streaming is essential for large datasets that don't fit in memory. This extends P1 to handle multiple batches in a single bulk copy session.

**Independent Test**: Stream 5 batches of 10K rows each, verify total row count and data integrity.

**Acceptance Scenarios**:

1. **Given** 5 `RecordBatch` values with 10K rows each, **When** they are streamed via successive `write_record_batch()` calls, **Then** all 50K rows are present in the destination table.

2. **Given** batches with varying row counts (1K, 50K, 100, 20K), **When** streamed, **Then** all rows are correctly inserted regardless of batch size variation.

---

### User Story 4 — Arrow Type Coverage for All SQL Server Types (Priority: P2)

Arrow `RecordBatch` data can contain columns mapped to the full range of SQL Server types. The crate must handle the complete type matrix.

**Why this priority**: Real-world tables have diverse schemas. Without full type coverage, users hit errors on common types.

**Independent Test**: Bulk-copy a batch with one column of each supported type; verify round-trip correctness by reading back.

**Acceptance Scenarios**:

1. **Given** a `RecordBatch` containing all supported type mappings (see Type Mapping table in Requirements), **When** bulk copied, **Then** all values are correctly stored and retrievable.

2. **Given** an Arrow column with an unsupported type (e.g., `ListArray`, `MapArray`), **When** bulk copy is attempted, **Then** a clear error message is returned indicating the unsupported type.

---

### User Story 5 — Schema Inference and Validation (Priority: P3)

When no explicit column mappings are provided, the crate should infer the Arrow→TDS type mapping from the destination table's metadata and validate compatibility with the RecordBatch schema.

**Why this priority**: Reduces boilerplate and catches schema mismatches early.

**Independent Test**: Attempt to bulk-copy a batch with mismatched column count or incompatible types; verify clear error messages.

**Acceptance Scenarios**:

1. **Given** a `RecordBatch` with columns matching the destination table by name, **When** no explicit mapping is specified, **Then** columns are auto-mapped by name and the operation succeeds.

2. **Given** a `RecordBatch` with an Int32 column mapped to a NVARCHAR destination, **When** validation runs, **Then** a clear error is returned before any data is sent.

3. **Given** a `RecordBatch` with fewer columns than the destination table, **When** extra destination columns are nullable, **Then** the operation succeeds with NULLs for unmapped columns.

---

### Edge Cases

- What happens when a `RecordBatch` contains `DictionaryArray` (dictionary-encoded strings)? Should transparently decode to NVARCHAR.
- How does the system handle Arrow `LargeUtf8` vs `Utf8` vs `Utf8View`? All should map to NVARCHAR.
- What happens when a DECIMAL column has precision > 38 in Arrow but SQL Server max is 38? Error with message.
- How are Arrow `Duration`, `Interval`, and `Union` types handled? Unsupported with clear error.
- What happens on network failure mid-bulk-copy? Transaction rollback behavior should match `BulkCopy` semantics.
- What about very large strings (>4000 chars) that require NVARCHAR(MAX)? Must use PLP encoding.
- What happens when the RecordBatch has zero columns? Error.

---

## Requirements *(mandatory)*

### Functional Requirements

#### Crate Structure

- **FR-001**: The crate MUST be named `mssql-arrow` and live at `mssql-arrow/` in the workspace root.
- **FR-002**: The crate MUST depend on `mssql-tds` (path dependency) and `arrow-*` crates (arrow-array, arrow-schema, arrow-buffer, arrow-cast). It MUST NOT require internal/`pub(crate)` access to `mssql-tds`.
- **FR-003**: The crate MUST be added to the workspace `members` list in the root `Cargo.toml`.
- **FR-004**: The crate MUST use Rust edition 2024 consistent with other workspace crates.
- **FR-005**: Every `.rs` file MUST include the Microsoft copyright header.

#### Bulk Copy Write Path (Arrow → SQL Server)

- **FR-010**: The crate MUST provide an `ArrowBulkCopy` struct that wraps `mssql_tds::connection::bulk_copy::BulkCopy` and accepts Arrow `RecordBatch` input.
- **FR-011**: `ArrowBulkCopy` MUST expose the same builder options as `BulkCopy`: `batch_size`, `timeout`, `check_constraints`, `fire_triggers`, `keep_identity`, `keep_nulls`, `table_lock`, `use_internal_transaction`.
- **FR-012**: The crate MUST implement the `BulkLoadRow` trait for an internal row type that pre-serializes Arrow column values to TDS wire bytes, using `StreamingBulkLoadWriter::write_raw_bytes()` for maximum throughput (the "pre-serialized" path proven in benchmarks).
- **FR-013**: Pre-serialization MUST iterate Arrow column arrays in tight columnar loops per the architecture proven in `pre_serialize_arrow_to_tds()`, producing one `Vec<u8>` per row containing all TDS wire bytes.
- **FR-014**: The crate MUST support nullable Arrow columns. When an Arrow value is null (via the array's null bitmap), the TDS null encoding appropriate for the column type MUST be written.
- **FR-015**: The crate MUST retrieve destination table metadata via `BulkCopy::retrieve_destination_metadata()` and use it to determine TDS type encodings.
- **FR-016**: The crate MUST support column mappings (by name and by ordinal) via `ColumnMapping`.

#### Query Read Path (SQL Server → Arrow)

- **FR-020**: The crate MUST provide an `ArrowQueryReader` that implements the `RowWriter` trait to decode query results directly into Arrow `RecordBatch` format.
- **FR-021**: `ArrowQueryReader` MUST use `ResultSet::next_row_into()` to bypass the `ColumnValues` intermediate representation.
- **FR-022**: `ArrowQueryReader` MUST support configurable batch sizes — accumulating N rows before producing a `RecordBatch`, to control memory usage.
- **FR-023**: `ArrowQueryReader` MUST infer Arrow schema from `ResultSet::get_metadata()` column metadata.
- **FR-024**: The `ArrowRowWriter` implementation currently behind `#[cfg(test)]` in `mssql-tds/src/datatypes/arrow_writer.rs` MUST be moved into the `mssql-arrow` crate as the production implementation.

#### Type Mapping

- **FR-030**: The crate MUST support the following Arrow ↔ SQL Server type mappings:

| Arrow Type | SQL Server Type | TDS Wire | Direction |
|---|---|---|---|
| `Boolean` | BIT | BITN | Read + Write |
| `UInt8` | TINYINT | INT1 / INTN | Read + Write |
| `Int16` | SMALLINT | INT2 / INTN | Read + Write |
| `Int32` | INT | INT4 / INTN | Read + Write |
| `Int64` | BIGINT | INT8 / INTN | Read + Write |
| `Float32` | REAL | FLT4 / FLTN | Read + Write |
| `Float64` | FLOAT | FLT8 / FLTN | Read + Write |
| `Decimal128(p,s)` | DECIMAL(p,s) / NUMERIC(p,s) | DECIMALN | Read + Write |
| `Utf8` / `LargeUtf8` | NVARCHAR / NCHAR / NTEXT | NVARCHAR | Read + Write |
| `Binary` / `LargeBinary` | VARBINARY / BINARY / IMAGE | VARBINARY | Read + Write |
| `Date32` | DATE | DATEN | Read + Write |
| `Time64(Microsecond)` | TIME | TIMEN | Read + Write |
| `Timestamp(Microsecond, None)` | DATETIME2 | DATETIME2N | Read + Write |
| `Timestamp(Microsecond, Some(tz))` | DATETIMEOFFSET | DATETIMEOFFSETN | Read + Write |
| `Timestamp(Microsecond, None)` | DATETIME | DATETIMEN | Read only |
| `Timestamp(Microsecond, None)` | SMALLDATETIME | DATETIMEN | Read only |
| `FixedSizeBinary(16)` | UNIQUEIDENTIFIER | GUID | Read + Write |
| `Float64` | MONEY / SMALLMONEY | MONEYN | Read only |
| `Utf8` | XML | XML (PLP) | Read + Write |
| `Utf8` | JSON | JSON (PLP) | Read + Write |
| `Utf8` | VARCHAR / CHAR / TEXT | VARCHAR | Read + Write |

- **FR-031**: For Arrow types not in the mapping table, the crate MUST return a `TdsResult::Err` with a message identifying the unsupported Arrow `DataType`.
- **FR-032**: For the write path, NVARCHAR strings MUST be encoded as UTF-16LE on the wire. The crate MUST write the byte count (not character count) as the length prefix.

#### Error Handling

- **FR-040**: All public methods MUST return `TdsResult<T>`.
- **FR-041**: Schema mismatches (column count, incompatible types) MUST be detected before sending data and reported with descriptive error messages.
- **FR-042**: The crate MUST propagate cancellation via `CancelHandle` consistent with `mssql-tds` patterns.

#### Performance

- **FR-050**: The write path MUST use columnar iteration over Arrow arrays (not row-major extraction) to maximize cache locality.
- **FR-051**: The write path MUST produce zero per-row heap allocations for fixed-size types (integers, floats, decimals, dates, UUIDs). String columns require a one-time UTF-16 encoding but MUST NOT create intermediate `SqlString` or `ColumnValues` objects.
- **FR-052**: The write path MUST use `write_raw_bytes()` for a single bulk write per row of pre-serialized TDS bytes.

### Key Entities

- **`ArrowBulkCopy`**: Builder-style entry point for bulk copy operations. Wraps `BulkCopy` and handles Arrow→TDS conversion.
- **`ArrowQueryReader`**: Wraps `ResultSet` iteration and produces `RecordBatch` output by implementing `RowWriter`.
- **`ArrowTdsSerializer`**: Internal module containing per-type Arrow→TDS serialization functions (the `tds_intn_i32`, `tds_nvarchar`, etc. functions from the benchmark, generalized).
- **`ArrowBulkLoadRow`**: Internal struct implementing `BulkLoadRow` that holds pre-serialized TDS bytes for a single row.

---

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Bulk copy of a 100K-row RecordBatch (INT, BIGINT, FLOAT, NVARCHAR(200), DECIMAL(18,2)) achieves ≥300 Krows/s throughput against SQL Server (consistent with the 363 Krows/s benchmark result, allowing for variance).
- **SC-002**: The bulk copy write path produces zero per-row heap allocations for non-string columns (verifiable via allocation profiling or by code inspection of the hot loop).
- **SC-003**: All 20+ Arrow↔SQL Server type mappings pass round-trip correctness tests (write via `ArrowBulkCopy`, read back via `ArrowQueryReader`, compare values).
- **SC-004**: The crate compiles with `cargo bclippy` (zero warnings) and passes `cargo bfmt`.
- **SC-005**: The crate has ≥85% test coverage for new code, consistent with the project's CI diff coverage target.
- **SC-006**: No new `pub(crate)` or internal API exposure is required in `mssql-tds` — the crate works entirely through existing public APIs.

---

## Technical Design Notes

### Crate Layout

```
mssql-arrow/
├── Cargo.toml
├── src/
│   ├── lib.rs                  # Public API: ArrowBulkCopy, ArrowQueryReader
│   ├── bulk_copy.rs            # ArrowBulkCopy builder, ArrowBulkLoadRow
│   ├── query_reader.rs         # ArrowQueryReader (RowWriter impl)
│   ├── serializer.rs           # Arrow→TDS per-type serialization functions
│   ├── type_mapping.rs         # Arrow↔SqlDbType mapping and validation
│   └── error.rs                # Arrow-specific error types (wraps TdsResult)
└── tests/
    ├── bulk_copy_integration.rs
    └── query_reader_integration.rs
```

### Dependency Graph

```
mssql-arrow
├── mssql-tds (path = "../mssql-tds")
├── arrow-array
├── arrow-schema
├── arrow-buffer
├── arrow-cast (for type coercions)
├── async-trait
└── tokio (runtime)
```

### Write Path Architecture

```
RecordBatch
    │
    ├── Retrieve destination metadata
    │   └── BulkCopy::retrieve_destination_metadata()
    │
    ├── Validate schema compatibility
    │   └── Arrow schema vs destination column types
    │
    ├── Pre-serialize: columnar iteration
    │   ├── For each column type, tight loop over Arrow array:
    │   │   ├── Int32Array  → [0x04][i32 LE] per row
    │   │   ├── Int64Array  → [0x08][i64 LE] per row
    │   │   ├── Float64Array→ [0x08][f64 LE] per row
    │   │   ├── Utf8Array   → [byte_count][UTF-16LE] per row
    │   │   ├── Decimal128  → [0x09][sign][8B LE] per row
    │   │   └── ... (all mapped types)
    │   └── Output: Vec<ArrowBulkLoadRow> with pre-serialized bytes
    │
    └── Execute via BulkCopy::write_to_server_zerocopy()
        └── Each ArrowBulkLoadRow::write_to_packet()
            └── writer.write_raw_bytes(&self.tds_bytes)
```

### Read Path Architecture

```
ResultSet (after execute)
    │
    ├── get_metadata() → infer Arrow Schema
    │
    ├── ArrowQueryReader implements RowWriter
    │   ├── Backed by per-column Arrow builders (Int32Builder, StringBuilder, etc.)
    │   ├── Each write_*() method appends to the appropriate builder
    │   └── end_row() increments row counter
    │
    ├── Loop: next_row_into(&mut arrow_reader)
    │   └── Decoder calls RowWriter methods directly (no ColumnValues)
    │
    └── When batch_size reached or result set exhausted:
        └── Finish builders → RecordBatch
```

### Null Handling (Write Path)

For each column type, when the Arrow array's null bitmap indicates a null at row index `i`:

- **Fixed-length nullable types (INTN, FLTN, BITN, MONEYN, DATETIMEN)**: Write `0x00` (zero-length byte)
- **Variable-length types (NVARCHAR, VARCHAR, VARBINARY)**: Write `0xFFFF` (VARNULL sentinel)
- **PLP types (NVARCHAR(MAX), VARBINARY(MAX), XML, JSON)**: Write PLP null marker (`0xFFFFFFFFFFFFFFFF`)
- **Date types (DATEN)**: Write `0x00`
- **Time-scale types (TIMEN, DATETIME2N, DATETIMEOFFSETN)**: Write `0x00`

### What Moves Out of `mssql-tds`

- The `ArrowRowWriter` implementation in `mssql-tds/src/datatypes/arrow_writer.rs` (currently `#[cfg(test)]`) moves to `mssql-arrow/src/query_reader.rs` as the production `RowWriter` implementation.
- The `pre_serialize_arrow_to_tds()` logic in `mssql-tds/tests/bench_arrow_bulk_copy.rs` moves to `mssql-arrow/src/serializer.rs`, generalized to handle all type mappings.
- The benchmark test `bench_arrow_bulk_copy.rs` remains in `mssql-tds` as a comparison baseline but the `mssql-arrow` crate gets its own integration tests.

### What Stays in `mssql-tds`

- `BulkCopy`, `BulkLoadRow`, `StreamingBulkLoadWriter` — unchanged public API
- `RowWriter` trait, `DefaultRowWriter` — unchanged
- `ResultSet::next_row_into()` — unchanged
- `ColumnValues`, `DecimalParts`, `SqlString` — unchanged (still used by non-Arrow callers)
- The criterion benchmark `benches/arrow_tds_bench.rs` can remain as dev-dependency test code

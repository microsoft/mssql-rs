# Feature Specification: Arrow Bulk Copy Crate (`mssql-arrow`)

**Feature Branch**: `001-arrow-bulk-copy-crate`  
**Created**: 2026-03-07  
**Status**: Draft  
**Input**: User description: "Develop a separate mssql-arrow crate with Arrow bulk copy capability that calls into mssql-tds for high-performance bulk insert of Arrow RecordBatch data to SQL Server and reading query results into Arrow RecordBatch format"

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Bulk Insert Arrow Data into SQL Server (Priority: P1)

A data engineer has tabular data in Apache Arrow columnar format — produced by tools like Polars, DuckDB, Pandas, or read from Parquet files — and needs to bulk-insert it into a SQL Server table. Today this requires manually converting each value through an intermediate representation, which halves throughput. The engineer wants a single call that takes their Arrow data and inserts it at maximum speed.

**Why this priority**: This is the primary use case. Benchmarks on this codebase have demonstrated a 2× throughput improvement when Arrow data is serialized directly to the wire format versus going through an intermediate value representation. This directly impacts ETL pipeline SLAs.

**Independent Test**: Create an Arrow dataset with common column types (integer, large integer, floating point, text, decimal), bulk-insert it into a SQL Server table, then query the table to verify all rows and values were inserted correctly.

**Acceptance Scenarios**:

1. **Given** an Arrow dataset with 100,000 rows and 5 columns (integer, large integer, float, text, decimal), **When** the user calls the bulk insert operation, **Then** all rows are inserted into the destination table with correct values.

2. **Given** an Arrow dataset with columns containing null values, **When** the bulk insert is performed, **Then** nulls are correctly stored in the destination table.

3. **Given** an Arrow dataset whose columns are in a different order than the destination table, **When** column mappings are provided (by name or position), **Then** data is inserted into the correct destination columns.

4. **Given** an Arrow dataset with zero rows, **When** the bulk insert is called, **Then** the operation completes successfully with zero rows affected.

5. **Given** a bulk insert in progress, **When** the user requests cancellation, **Then** the operation stops and the partial transaction is rolled back per the existing bulk copy semantics.

---

### User Story 2 - Read Query Results as Arrow Data (Priority: P1)

A data analyst executes a SQL query and wants the results delivered directly as Arrow columnar data for downstream analytics processing (DataFrames, columnar transforms, Parquet export). Today, results come back as individual row values that must be manually assembled into Arrow format. The analyst wants query results to arrive as ready-to-use Arrow batches.

**Why this priority**: Reading results into Arrow completes the round-trip. Analytics workflows consume Arrow natively, so eliminating the intermediate per-row value representation avoids redundant work and reduces memory pressure. The infrastructure for this (the `RowWriter` decode sink trait and a test-only Arrow implementation) already exists in the codebase.

**Independent Test**: Execute a query returning known data with diverse column types, collect the results as Arrow batches, and verify the schema and values match the expected output.

**Acceptance Scenarios**:

1. **Given** a query returning rows with integer, float, text, decimal, date, datetime, and UUID columns, **When** results are read as Arrow batches, **Then** each column has the correct Arrow type and all values match the source data.

2. **Given** a query returning 250,000 rows with a batch size of 10,000, **When** all batches are consumed, **Then** exactly 25 batches are produced, each with 10,000 rows, and all data is accounted for.

3. **Given** a query result containing null values, **When** read as Arrow data, **Then** Arrow null indicators are correctly set for each null value.

4. **Given** a query that produces multiple result sets, **When** the reader is used, **Then** each result set can be independently consumed as Arrow batches.

---

### User Story 3 - Stream Large Datasets in Chunks (Priority: P2)

A data pipeline reads a large dataset in chunks (e.g., scanning a multi-gigabyte Parquet file partition by partition) and needs to stream each chunk into SQL Server without accumulating the entire dataset in memory. The user wants to feed Arrow batches one at a time into a single continuous bulk insert session.

**Why this priority**: Memory-bounded streaming is essential for production ETL workloads. This extends the P1 bulk insert to handle incremental data sources without requiring the full dataset upfront.

**Independent Test**: Stream 5 Arrow batches of 10,000 rows each into a single table, then verify the total row count and data integrity.

**Acceptance Scenarios**:

1. **Given** 5 Arrow batches with 10,000 rows each, **When** they are streamed into a table via successive write calls, **Then** all 50,000 rows are present in the destination table with correct values.

2. **Given** Arrow batches of varying sizes (1,000; 50,000; 100; 20,000 rows), **When** streamed into a single bulk insert session, **Then** all rows are correctly inserted regardless of batch size variation.

---

### User Story 4 - Full SQL Server Type Support (Priority: P2)

Real-world SQL Server tables use diverse column types beyond the basic integer/float/string set. The bulk copy and query reader must handle the full range of common SQL Server data types, including dates, times, timestamps with timezone, binary data, UUIDs, money, XML, JSON, and vectors.

**Why this priority**: Without comprehensive type coverage, users will encounter errors on real production table schemas, limiting adoption.

**Independent Test**: Create a table with one column of each supported SQL Server type, bulk insert an Arrow batch with matching types, read the data back as Arrow, and verify round-trip correctness for every type.

**Acceptance Scenarios**:

1. **Given** an Arrow dataset containing columns for all supported type mappings (boolean, integers, floats, decimals, strings, binary, dates, times, timestamps, UUIDs, XML, JSON), **When** bulk inserted and read back, **Then** all values survive the round trip without data loss or corruption.

2. **Given** an Arrow dataset with a column type that has no SQL Server equivalent (e.g., nested lists, maps, unions), **When** bulk insert is attempted, **Then** a clear error message names the unsupported type before any data is sent.

---

### User Story 5 - Automatic Schema Matching (Priority: P3)

When arrow column names match the destination table columns, the user wants the system to automatically align them without requiring explicit column mappings. When there's a mismatch, the user wants a clear error before any data is transmitted.

**Why this priority**: Reduces boilerplate for the common case and provides fail-fast behavior for mismatches.

**Independent Test**: Attempt bulk insert with matching column names (should work), mismatched column count (should error), and incompatible types (should error with descriptive message).

**Acceptance Scenarios**:

1. **Given** an Arrow dataset whose column names match the destination table, **When** no explicit column mappings are specified, **Then** columns are automatically matched by name and the insert succeeds.

2. **Given** an Arrow dataset with an integer column targeting a text-only destination column, **When** schema validation runs, **Then** a clear error is returned identifying the type incompatibility before any data transfer.

3. **Given** an Arrow dataset with fewer columns than the destination table, **When** the unmatched destination columns are nullable, **Then** the insert succeeds with null values for unmapped columns.

---

### Edge Cases

- Arrow dictionary-encoded string columns should be transparently handled as text values.
- Multiple Arrow string representations (standard, large, view-based) should all map to SQL Server text columns.
- Decimal columns with precision exceeding SQL Server's maximum of 38 should produce a clear error.
- Very large strings (exceeding 4,000 characters) and MAX-type columns must be serialized using PLP (Partially Length-Prefixed) wire encoding.
- Network failure mid-bulk-copy should follow existing transaction rollback behavior.
- Multi-batch streaming failure follows existing bulk copy semantics: previously flushed rows may persist, the failed batch and subsequent batches are not sent, and the error reports the number of rows successfully copied.
- Arrow datasets with zero columns should produce a clear error.
- Timestamps with various timezone representations should be handled consistently.

## Clarifications

### Session 2026-03-07

- Q: Should the write path support MAX-type destination columns (NVARCHAR(MAX), VARBINARY(MAX)) via PLP encoding? → A: Yes, support MAX columns via PLP encoding for real-world schema compatibility.
- Q: When streaming multiple RecordBatches, if a mid-stream batch fails, what happens to already-sent rows? → A: Follow existing bulk copy semantics — previously flushed rows may persist; failed and subsequent batches are not sent; error includes rows-copied count.
- Q: Should ArrowQueryReader use result metadata for upfront schema inference instead of lazy type discovery? → A: Yes, use metadata for upfront inference; produce correctly-typed (but all-null) arrays for null-only columns.
- Q: Should the original #[cfg(test)] ArrowRowWriter in mssql-tds be removed or kept after extraction? → A: Keep independent copy in mssql-tds unchanged. mssql-arrow has its own production copy with its own integration tests. Future goal: remove all Arrow dependencies from mssql-tds entirely.
- Q: Which capabilities are explicitly out of scope for this iteration? → A: All four: implicit type coercion, Arrow IPC/Flight integration, connection pooling/multi-connection bulk copy, and write path for DATETIME/SMALLDATETIME/MONEY.

## Out of Scope

The following are explicitly **not** part of this iteration:

- **Implicit type coercion**: Only exact Arrow↔SQL Server type matches are supported (e.g., Arrow Int32 → INT). No automatic widening (Int32 → BIGINT) or narrowing. Mismatched types produce an error.
- **Arrow IPC / Flight integration**: No reading from Arrow IPC streams, Flight endpoints, or Feather files. Input is `RecordBatch` only.
- **Connection pooling / multi-connection bulk copy**: The API takes a single `&mut TdsClient`. Parallelism across connections is the caller's responsibility.
- **Write path for DATETIME / SMALLDATETIME / MONEY / SMALLMONEY**: Per FR-031, these are read-only mappings (SQL Server → Arrow). Writing Arrow data to these destination column types is not supported in this iteration.

## Requirements *(mandatory)*

### Functional Requirements

#### Crate Boundaries

- **FR-001**: The Arrow bulk copy capability MUST be provided as a separate module (`mssql-arrow`) that is distinct from the core TDS protocol library, keeping Arrow-specific dependencies isolated.
- **FR-002**: The module MUST integrate with the existing TDS protocol library through its established public interfaces only, without requiring changes to internal APIs.
- **FR-003**: The module MUST follow the same code conventions, licensing, and quality standards as the rest of the project (copyright headers, edition consistency, lint rules).

#### Bulk Insert (Arrow → SQL Server)

- **FR-010**: The module MUST provide a builder-style entry point for bulk inserting Arrow data that exposes the same configuration options as the existing bulk copy API (batch size, timeout, constraints, triggers, identity, nulls, table locking, internal transactions).
- **FR-011**: The module MUST convert Arrow columnar data to the TDS wire format using direct columnar iteration — processing each column's contiguous memory buffer in a tight loop rather than extracting values row by row — to maximize throughput.
- **FR-012**: The module MUST produce zero per-row heap allocations for fixed-size types (integers, floats, decimals, dates, UUIDs) during the conversion from Arrow to wire format.
- **FR-013**: The module MUST retrieve the destination table's column metadata and use it to determine the correct wire encoding for each column.
- **FR-014**: The module MUST correctly handle nullable Arrow columns by writing the appropriate null encoding for each column type when the Arrow null bitmap indicates a null value.
- **FR-015**: The module MUST support explicit column mapping (by name and by position) between Arrow columns and destination table columns.
- **FR-016**: For text columns, the module MUST encode strings as UTF-16LE on the wire and write the byte count (not character count) as the length prefix.

#### Query Reader (SQL Server → Arrow)

- **FR-020**: The module MUST provide a reader that decodes SQL Server query results directly into Arrow columnar format, bypassing the intermediate per-row value representation.
- **FR-021**: The reader MUST support configurable batch sizes, accumulating rows into Arrow builders and producing a complete Arrow batch when the threshold is reached or the result set is exhausted.
- **FR-022**: The reader MUST infer the Arrow schema upfront from the query result's `ColumnMetadata` (available via `ResultSet::get_metadata()`), producing correctly-typed Arrow arrays even for columns that are entirely null. Lazy type discovery is not used.
- **FR-023**: The existing test-only Arrow row writer implementation in the TDS library serves as the reference for the production implementation in this module. The mssql-arrow crate MUST have an independent copy (not a dev-dependency relationship). The `#[cfg(test)]` copy in mssql-tds remains unchanged for now. Future goal: remove all Arrow dependencies (arrow-array, arrow-schema, arrow-buffer) from mssql-tds once mssql-arrow has full integration test coverage for the exposed APIs.

#### Type Coverage

- **FR-030**: The module MUST support bidirectional (read + write) mapping for these type pairs:

  - Boolean ↔ BIT
  - 8-bit unsigned integer ↔ TINYINT
  - 16-bit integer ↔ SMALLINT
  - 32-bit integer ↔ INT
  - 64-bit integer ↔ BIGINT
  - 32-bit float ↔ REAL
  - 64-bit float ↔ FLOAT
  - Decimal (with precision and scale) ↔ DECIMAL / NUMERIC
  - Text (UTF-8) ↔ NVARCHAR / NCHAR / VARCHAR / CHAR (including MAX variants via PLP encoding)
  - Binary ↔ VARBINARY / BINARY (including MAX variants via PLP encoding)
  - Date ↔ DATE
  - Time (microsecond precision) ↔ TIME
  - Timestamp (microsecond, no timezone) ↔ DATETIME2
  - Timestamp (microsecond, with timezone) ↔ DATETIMEOFFSET
  - Fixed 16-byte binary ↔ UNIQUEIDENTIFIER
  - Text ↔ XML
  - Text ↔ JSON

- **FR-031**: The module MUST support read-only (SQL Server → Arrow) mapping for:

  - DATETIME → Timestamp (microsecond)
  - SMALLDATETIME → Timestamp (microsecond)
  - MONEY / SMALLMONEY → 64-bit float

- **FR-032**: For any Arrow type not in the supported mapping, the module MUST return an error with a message identifying the unsupported type.

#### Error Handling

- **FR-040**: All public operations MUST return the standard result type used by the TDS library.
- **FR-041**: Schema mismatches (column count mismatch, incompatible types) MUST be detected before data transmission begins and reported with descriptive error messages.
- **FR-042**: The module MUST propagate cancellation signals consistent with the existing cancellation patterns in the TDS library.

### Key Entities

- **Arrow Bulk Copy Builder**: Entry point for bulk insert operations. Wraps the existing bulk copy builder and adds Arrow-specific data conversion. Attributes: destination table name, batch size, timeout, constraint/trigger options, column mappings.
- **Arrow Query Reader**: Wraps result set iteration and produces Arrow batches. Attributes: batch size, inferred schema, column builders.
- **Arrow-to-Wire Serializer**: Internal component that performs the per-type conversion from Arrow columnar buffers to TDS wire bytes. Handles null encoding, type coercion, and string transcoding.
- **Type Mapping Registry**: Internal component that maps between Arrow data types and SQL Server data types, validating compatibility and determining wire encoding parameters.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Bulk inserting 100,000 rows (with integer, large integer, float, text, and decimal columns) completes in under 350ms against a SQL Server instance, representing at least 1.5× throughput improvement over the existing intermediate-value path.
- **SC-002**: All supported type mappings (20+) pass round-trip correctness tests: data written via bulk insert and read back via query reader produces identical values.
- **SC-003**: The bulk insert path produces zero per-row heap allocations for datasets containing only fixed-size column types, verifiable by code inspection of the serialization hot path.
- **SC-004**: The module compiles cleanly under the project's lint rules (zero warnings) and passes formatting checks.
- **SC-005**: New code achieves ≥85% test coverage, consistent with the project's CI diff coverage target.
- **SC-006**: The module operates entirely through the TDS library's existing public API surface — no internal API changes are required in the core library.

## Assumptions

- The existing `mssql-tds` public API surface (`BulkCopy`, `BulkLoadRow`, `StreamingBulkLoadWriter`, `RowWriter`, `ResultSet`) is stable and sufficient for the Arrow module's needs.
- Arrow crate versions will be pinned to the `57.x` series already used in dev-dependencies.
- The pre-serialized bulk write approach (proven at 363 Krows/s in benchmarks) is the optimal architecture for the write path.
- The `ArrowRowWriter` test implementation in `mssql-tds` is functionally correct and needs only extraction and minor hardening for production use.

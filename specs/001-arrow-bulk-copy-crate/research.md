# Research: Arrow Bulk Copy Crate

**Feature**: 001-arrow-bulk-copy-crate  
**Date**: 2026-03-07

## R1: Write Path Architecture — Pre-Serialization vs Streaming

**Decision**: Pre-serialized columnar iteration (Path A from benchmarks)

**Rationale**: Benchmarks in `bench_arrow_bulk_copy.rs` demonstrate 363 Krows/s with pre-serialization vs 174 Krows/s with `ColumnValues` materialization (2.09× throughput). The pre-serialized path iterates each Arrow column's contiguous memory buffer in a tight loop, appending TDS wire bytes to per-row buffers, then writes each row via `write_raw_bytes()` with zero per-value dispatch.

**Alternatives considered**:
- **Path B (Materialized rows)**: Arrow → `Vec<MaterializedRow>` → `write_column_value()`. Incurs per-value enum dispatch and ColumnValues boxing. 174 Krows/s.
- **Path C (Streaming)**: Direct unchecked writes to packet buffer during iteration. Slightly faster than Path A in microbenchmarks but requires unsafe packet buffer access patterns and tighter coupling with StreamingBulkLoadWriter internals.

## R2: Null Encoding Per TDS Type Class

**Decision**: Check Arrow null bitmap per column, emit type-appropriate null wire bytes

**Rationale**: The benchmark code has no null handling — all columns are non-nullable. Production code must check `array.is_null(row_index)` and emit the correct null sentinel per TDS type class.

**Null wire encoding by type class**:

| TDS Type Class | Null bytes | Non-null prefix |
|---|---|---|
| INTN (i32) | `[0x00]` | `[0x04]` + 4 bytes LE |
| INTN (i64) | `[0x00]` | `[0x08]` + 8 bytes LE |
| FLTN (f32) | `[0x00]` | `[0x04]` + 4 bytes LE |
| FLTN (f64) | `[0x00]` | `[0x08]` + 8 bytes LE |
| BITN | `[0x00]` | `[0x01]` + 1 byte |
| NVARCHAR | `[0xFF][0xFF]` | `[byte_count u16 LE]` + UTF-16LE data |
| DECIMALN | `[0x00]` | `[len]` + sign + value bytes |
| DATEN | `[0x00]` | `[0x03]` + 3 bytes LE |
| TIMEN | `[0x00]` | `[len]` + time bytes (scale-dependent) |
| DATETIME2N | `[0x00]` | `[len]` + time + date bytes |
| DATETIMEOFFSETN | `[0x00]` | `[len]` + time + date + offset bytes |
| UNIQUEIDENTIFIER | `[0x00]` | `[0x10]` + 16 bytes (TDS byte-swapped) |
| VARBINARY | `[0xFF][0xFF]` | `[byte_count u16 LE]` + raw bytes |

## R3: Public API Surface for Integration

**Decision**: Integrate exclusively through mssql-tds public API — no `pub(crate)` changes needed

**Rationale**: Investigation of the mssql-tds public API confirms all required interfaces are already public:

- **Write path**: `BulkCopy::new(&mut TdsClient, table)` → builder chain → `write_to_server_zerocopy(rows)`. The `BulkLoadRow` trait's `write_to_packet(&self, &mut StreamingBulkLoadWriter, &mut usize)` gives access to `write_raw_bytes()`.
- **Read path**: `ResultSet::next_row_into(&mut dyn RowWriter)` feeds decoded values to any `RowWriter` impl. All 25 `RowWriter` methods are pub.
- **Metadata**: `BulkCopy::retrieve_destination_metadata()` returns `Vec<BulkCopyColumnMetadata>` with all type info needed for serialization decisions. `ResultSet::get_metadata()` returns `Vec<ColumnMetadata>` for read-path schema inference.
- **Client construction**: `TdsConnectionProvider::create_client()` is pub (also re-exported at crate root).
- **Errors**: `TdsResult<T>`, `Error` enum, `BulkCopyError` are all pub.
- **Cancellation**: `CancelHandle` is pub with `new()`, `cancel()`, `child_handle()`.

**Key design constraint**: `TdsClient` constructor is `pub(crate)` — external crates receive it from `TdsConnectionProvider`, not construct it directly. This is fine since mssql-arrow receives `&mut TdsClient` from users who obtained it through the provider.

**Independence requirement**: mssql-arrow will NOT be added as a dev-dependency to mssql-tds. The two crates are independent within the workspace. Future goal is to remove arrow-array, arrow-schema, arrow-buffer from mssql-tds entirely once mssql-arrow has equivalent integration test coverage.

## R4: ArrowRowWriter Extraction

**Decision**: Create an independent copy of `ArrowRowWriter` in `mssql-arrow/src/query_reader.rs` as production code. The `#[cfg(test)]` copy in mssql-tds remains unchanged.

**Rationale**: The user explicitly chose to keep both copies independent (no dev-dependency relationship). mssql-arrow has its own integration tests. Future goal: remove all Arrow dependencies from mssql-tds once mssql-arrow has equivalent coverage.

**Changes needed during extraction**:
- Remove `#[cfg(test)]` gating
- Change imports from `crate::` to `mssql_tds::`
- Replace lazy type discovery with upfront metadata-based schema inference (see R10)
- Add `ArrowQueryReader` wrapper that manages batch accumulation (configurable batch_size, produces RecordBatch when threshold reached or result set exhausted)
- Helper functions (`decimal_parts_to_i128`, `tds_date_to_arrow_date32`, `sql_time_to_micros`, etc.) move as-is

**Gaps to address**:
- VECTOR uses `format!("{val:?}")` (debug format) — needs proper serialization format
- MONEY/SMALLMONEY → Float64 loses precision past 2^53/10^4 — acceptable for P2 (read-only mapping per spec)

## R5: Decimal Precision-Dependent Wire Length

**Decision**: Implement precision-aware length calculation instead of hardcoding

**Rationale**: The benchmark hardcodes `length=9` (valid only for precision 10–19). TDS decimal wire encoding uses precision-dependent byte lengths:

| Precision range | Value bytes | Total length byte |
|---|---|---|
| 1–9 | 4 | 0x05 |
| 10–19 | 8 | 0x09 |
| 20–28 | 12 | 0x0D |
| 29–38 | 16 | 0x11 |

The length is 1 (sign byte) + value bytes. Arrow `Decimal128` always stores as i128 (16 bytes), so the serializer must truncate to the appropriate byte count based on the destination column's declared precision from `BulkCopyColumnMetadata`.

## R6: Type Mapping Completeness

**Decision**: Implement 20+ bidirectional mappings per spec FR-030, plus 4 read-only mappings per FR-031

**Rationale**: Cross-referencing the spec requirements with the existing `ArrowRowWriter` coverage:

| Mapping | ArrowRowWriter has it | Serializer needs it | Status |
|---|---|---|---|
| Boolean ↔ BIT | Yes | Yes | Both paths need impl |
| UInt8 ↔ TINYINT | Yes | Yes | Both paths need impl |
| Int16 ↔ SMALLINT | Yes | Yes | Both paths need impl |
| Int32 ↔ INT | Yes | Yes (benchmark has it) | Ready |
| Int64 ↔ BIGINT | Yes | Yes (benchmark has it) | Ready |
| Float32 ↔ REAL | Yes | Yes | Both paths need impl |
| Float64 ↔ FLOAT | Yes | Yes (benchmark has it) | Ready |
| Decimal128 ↔ DECIMAL/NUMERIC | Yes | Yes (benchmark has p18) | Needs precision generalization |
| Utf8 ↔ NVARCHAR/NCHAR | Yes | Yes (benchmark has it) | Ready |
| Binary ↔ VARBINARY | Yes | Yes | Write path needs impl |
| Date32 ↔ DATE | Yes | Yes | Write path needs impl |
| Time64 ↔ TIME | Yes | Yes | Write path needs impl |
| Timestamp ↔ DATETIME2 | Yes | Yes | Write path needs impl |
| Timestamp+tz ↔ DATETIMEOFFSET | Yes | Yes | Write path needs impl |
| FixedSizeBinary16 ↔ UNIQUEIDENTIFIER | Yes | Yes | Write path needs impl |
| Utf8 ↔ XML | Yes | Yes | Write path needs impl |
| Utf8 ↔ JSON | Yes | Yes | Write path needs impl |
| DATETIME → Timestamp | Yes (read-only) | N/A | Read-only, done |
| SMALLDATETIME → Timestamp | Yes (read-only) | N/A | Read-only, done |
| MONEY → Float64 | Yes (read-only) | N/A | Read-only, done |
| SMALLMONEY → Float64 | Yes (read-only) | N/A | Read-only, done |

**Alternatives considered**: Mapping MONEY to Decimal128 for lossless representation — deferred as MONEY is read-only per spec.

## R10: PLP Encoding for MAX-Type Columns

**Decision**: Support PLP (Partially Length-Prefixed) wire encoding for NVARCHAR(MAX), VARCHAR(MAX), and VARBINARY(MAX) destination columns

**Rationale**: Clarification Q1 confirmed MAX columns must be supported. Real-world tables frequently use MAX types for documents, JSON payloads, and binary blobs. Without PLP support, the crate would error on a large fraction of production schemas.

**Wire format (from mssql-tds `TdsValueSerializer`):**

| Scenario | Wire bytes |
|---|---|
| Non-null PLP | `[PLP_UNKNOWN_LEN 8B: 0xFFFFFFFFFFFFFFFE]` + `[chunk_len 4B: N as u32]` + `[data NB]` + `[PLP_TERMINATOR 4B: 0x00000000]` |
| Null PLP | `[PLP_NULL 8B: 0xFFFFFFFFFFFFFFFF]` |
| Non-null non-PLP var | `[byte_count 2B: u16]` + `[data]` |
| Null non-PLP var | `[VARNULL 2B: 0xFFFF]` |

PLP overhead is 16 bytes per value (vs 2 bytes for non-PLP), but this is necessary for correctness.

**Detection**: Check `BulkCopyColumnMetadata.length_type.is_plp()` — returns true when the destination column is a MAX type. The serializer branches on this to emit PLP framing or standard variable-length encoding.

**COLMETADATA wire**: PLP columns write `0xFFFF` as the 2-byte MaxLength sentinel in the bulk load header. This is already handled by `StreamingBulkLoadWriter::write_colmetadata()` in mssql-tds.

**Impact on `write_raw_bytes()`**: Since `write_raw_bytes()` has no PLP awareness, the pre-serialized row buffers must include the full PLP framing (8-byte header, 4-byte chunk length, data, 4-byte terminator) for MAX-type columns. The serializer module handles this.

## R11: Upfront Schema Inference from ColumnMetadata

**Decision**: Use `ResultSet::get_metadata()` for upfront Arrow schema inference instead of lazy type discovery

**Rationale**: Clarification Q3 confirmed upfront inference. This produces deterministic schemas regardless of data content (no NullArray for all-null columns), and the schema is known before any rows are consumed.

**Mapping function**: `ColumnMetadata` → Arrow `DataType` via match on `col.data_type` (TdsDataType enum):

| TdsDataType | Arrow DataType | Disambiguation |
|---|---|---|
| Int1 | UInt8 | — |
| Int2 | Int16 | — |
| Int4 | Int32 | — |
| Int8 | Int64 | — |
| IntN | UInt8/Int16/Int32/Int64 | `type_info.length`: 1→UInt8, 2→Int16, 4→Int32, 8→Int64 |
| Bit / BitN | Boolean | — |
| Flt4 | Float32 | — |
| Flt8 | Float64 | — |
| FltN | Float32/Float64 | `type_info.length`: 4→Float32, 8→Float64 |
| DecimalN / NumericN | Decimal128(p, s) | Extract from `VarLenPrecisionScale` variant |
| Money / Money4 / MoneyN | Float64 | Read-only mapping |
| DateN | Date32 | — |
| TimeN | Time64(Microsecond) | — |
| DateTime2N | Timestamp(μs, None) | — |
| DateTimeOffsetN | Timestamp(μs, "+00:00") | — |
| DateTime / DateTim4 / DateTimeN | Timestamp(μs, None) | — |
| NVarChar / NChar / BigVarChar / BigChar / Text / NText | Utf8 | PLP or sized — same Arrow type |
| BigVarBinary / BigBinary / VarBinary / Binary / Image | Binary | PLP or sized — same Arrow type |
| Guid | FixedSizeBinary(16) | — |
| Xml / Json | Utf8 | Always PLP |
| Vector | Utf8 | — |

**Nullable**: `col.is_nullable()` → `Field::new(name, type, nullable)`

**Column builders are initialized upfront**: Instead of starting as `Uninitialized`, each column's Arrow builder is created immediately based on the resolved DataType. Nulls on any column append to the correctly-typed builder rather than requiring deferred type resolution.

## R12: Multi-Batch Streaming Failure Semantics

**Decision**: Follow existing bulk copy semantics — previously flushed rows may persist; error reports rows-copied count

**Rationale**: Clarification Q2 confirmed this approach. The existing `BulkCopy::write_to_server_zerocopy()` treats each call as a single bulk insert operation. SQL Server commits or rolls back based on transaction context. Adding new transaction management would require changes to mssql-tds internals, violating the public-API-only constraint.

**Error reporting**: On failure, `BulkCopyResult` already carries `rows_affected`. The `write_batches()` method will track cumulative rows-copied across batches and include this count in the error context.

## R13: Out-of-Scope Items

**Decision**: Explicitly exclude implicit type coercion, Arrow IPC/Flight, connection pooling, and write path for DATETIME/SMALLDATETIME/MONEY

**Rationale**: Clarification Q5 confirmed all four are out of scope. This prevents scope creep and keeps the initial implementation focused on the core read/write paths with exact type matching.

## R7: Arrow String Type Variants

**Decision**: Handle `Utf8`, `LargeUtf8`, and `Utf8View` transparently as NVARCHAR text

**Rationale**: Arrow has three string representations. All store UTF-8 data but differ in offset/layout:
- `Utf8` (StringArray): 32-bit offsets, standard
- `LargeUtf8` (LargeStringArray): 64-bit offsets, for strings > 2GB total
- `Utf8View` (StringViewArray): view-based, introduced in Arrow 53+

All three should map to NVARCHAR on the wire (UTF-16LE encoding). The serializer dispatches based on Arrow DataType and calls the appropriate downcast.

Dictionary-encoded string columns (`Dictionary(_, Utf8)`) should be resolved to plain strings during serialization — iterate the dictionary indices and look up values.

## R8: Workspace Integration

**Decision**: Add `mssql-arrow` as a workspace member in root `Cargo.toml`

**Rationale**: Follows the existing pattern of `mssql-tds`, `mssql-js`, `mssql-tds-cli`, `mssql-mock-tds`. The crate will:
- Use Rust edition 2024 (matching all workspace members)
- Depend on `mssql-tds` via workspace path dependency
- Depend on `arrow-array`, `arrow-schema`, `arrow-buffer` at version `57` (matching mssql-tds dev-dependencies)
- Be included in `cargo bclippy`, `cargo bfmt`, `cargo btest` workspace commands
- Use the same CI profile (`.config/nextest.toml`)

## R9: Crate API Design Approach

**Decision**: Builder pattern for `ArrowBulkCopy`, iterator-based consumption for `ArrowQueryReader`

**Rationale**: 
- **Bulk copy**: Mirrors `BulkCopy` builder API — `ArrowBulkCopy::new(&mut TdsClient, table)` with chainable option methods, then `write_batch(batch)` / `write_batches(batches)` as the terminal operation. Internally constructs `BulkCopy`, retrieves metadata, creates serializer, pre-serializes rows, calls `write_to_server_zerocopy`.
- **Query reader**: `ArrowQueryReader::new(batch_size)` creates a `RowWriter` implementation. After executing a query, user calls `client.next_row_into(&mut reader)` in a loop, then `reader.finish()` to get the accumulated `RecordBatch`. Higher-level convenience: `ArrowQueryReader::read_all(&mut client, batch_size)` that returns `Vec<RecordBatch>`.

**Alternatives**: 
- Trait-based abstraction over different batch sources — rejected as overengineered for the current scope.
- Async stream of RecordBatch — deferred to future iteration; users can loop over `next_row_into` today.

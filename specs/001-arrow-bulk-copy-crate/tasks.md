# Tasks: Arrow Bulk Copy Crate (`mssql-arrow`)

**Input**: Design documents from `/specs/001-arrow-bulk-copy-crate/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/api.md, quickstart.md

**Organization**: Tasks grouped by user story for independent implementation and testing.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks)
- **[Story]**: US1–US5, maps to user stories from spec.md

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Create mssql-arrow crate skeleton and workspace integration

- [X] T001 Create mssql-arrow/Cargo.toml with edition 2024, path dep on mssql-tds, arrow-array 57, arrow-schema 57, arrow-buffer 57, async-trait, tracing, thiserror, tokio dependencies per plan.md
- [X] T002 Add mssql-arrow to workspace members list in root Cargo.toml
- [X] T003 [P] Create mssql-arrow/src/lib.rs with module declarations (bulk_copy, query_reader, serializer, type_mapping, error) and public re-exports per contracts/api.md

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Core infrastructure that ALL user stories depend on

**⚠️ CRITICAL**: No user story work can begin until this phase is complete

- [X] T004 Implement ArrowError enum with all variants (UnsupportedArrowType, TypeMismatch, ColumnCountMismatch, PrecisionOverflow, StringTruncation, EmptySchema, ArrowError) and From conversion to mssql_tds Error in mssql-arrow/src/error.rs
- [X] T005 [P] Create TypeMappingRegistry struct and ResolvedTypeMapping struct with fields (source_index, dest_index, arrow_type, sql_type, precision, scale, max_length, is_nullable, is_plp) in mssql-arrow/src/type_mapping.rs
- [X] T006 [P] Implement ArrowBulkLoadRow struct with BulkLoadRow trait impl — stores pre-serialized Vec<u8>, calls write_raw_bytes() and advances column_index in mssql-arrow/src/bulk_copy.rs

**Checkpoint**: Crate compiles with empty module stubs. Error types, mapping structs, and BulkLoadRow wrapper ready.

---

## Phase 3: User Story 1 — Bulk Insert Arrow Data (Priority: P1) 🎯 MVP

**Goal**: Bulk insert Arrow RecordBatch into SQL Server via pre-serialized columnar iteration at ≥300 Krows/s

**Independent Test**: Create Arrow dataset with 5 columns (i32, i64, f64, utf8, decimal), bulk-insert into SQL Server, query back to verify all rows and values

### Implementation for User Story 1

- [X] T007 [P] [US1] Implement serialize_i32 and serialize_i64 functions with INTN null encoding ([0x00] null, [len][value LE] non-null) in mssql-arrow/src/serializer.rs
- [X] T008 [P] [US1] Implement serialize_f64 function with FLTN null encoding in mssql-arrow/src/serializer.rs
- [X] T009 [P] [US1] Implement serialize_nvarchar function — UTF-8→UTF-16LE transcoding, u16 byte-count length prefix, [0xFF][0xFF] null sentinel in mssql-arrow/src/serializer.rs
- [X] T010 [P] [US1] Implement serialize_decimal function with precision-dependent wire lengths (p1-9→5B, p10-19→9B, p20-28→13B, p29-38→17B), sign byte, value truncation from i128 in mssql-arrow/src/serializer.rs
- [X] T011 [US1] Implement serialize_batch() orchestrator — iterate RecordBatch columns, dispatch to per-type serializers based on Arrow DataType, build Vec<Vec<u8>> of per-row pre-serialized buffers in mssql-arrow/src/serializer.rs
- [X] T012 [P] [US1] Implement type mapping resolution for core types (Int32↔INT, Int64↔BIGINT, Float64↔FLOAT, Utf8↔NVARCHAR, Decimal128↔DECIMAL) with validation and error reporting in mssql-arrow/src/type_mapping.rs
- [X] T013 [US1] Implement ArrowBulkCopy builder struct with chainable option methods (batch_size, timeout, check_constraints, fire_triggers, keep_identity, keep_nulls, table_lock, use_internal_transaction, add_column_mapping) in mssql-arrow/src/bulk_copy.rs
- [X] T014 [US1] Implement write_batch() terminal operation — retrieve destination metadata via BulkCopy::retrieve_destination_metadata(), build TypeMappingRegistry, call serialize_batch(), wrap rows as ArrowBulkLoadRow, call write_to_server_zerocopy() in mssql-arrow/src/bulk_copy.rs

**Checkpoint**: Single RecordBatch (i32, i64, f64, utf8, decimal including nulls) bulk-inserts into SQL Server correctly.

---

## Phase 4: User Story 2 — Read Query Results as Arrow (Priority: P1)

**Goal**: Decode SQL Server query results directly into Arrow RecordBatch via RowWriter trait with upfront metadata-based schema inference

**Independent Test**: Execute query returning diverse types (int, float, text, decimal, date, datetime, UUID), collect as Arrow batches, verify schema and values match

### Implementation for User Story 2

- [X] T015 [P] [US2] Implement column_metadata_to_data_type() function — match on TdsDataType with IntN/FltN disambiguation by type_info.length, DecimalN by VarLenPrecisionScale, nullable via is_nullable() in mssql-arrow/src/type_mapping.rs
- [X] T016 [US2] Implement ColumnBuilder enum with all 15 variants (Boolean, UInt8, Int16, Int32, Int64, Float32, Float64, Decimal128, Utf8, Binary, Date32, Time64Microsecond, TimestampMicrosecond, TimestampMicrosecondUtc, FixedSizeBinary16) initialized upfront from DataType in mssql-arrow/src/query_reader.rs
- [X] T017 [US2] Implement ArrowQueryReader::from_metadata() — resolve Schema from &[ColumnMetadata] using column_metadata_to_data_type(), create ColumnBuilders, store column names in mssql-arrow/src/query_reader.rs
- [X] T018 [US2] Implement RowWriter trait for ArrowQueryReader — all 25 methods (write_null through end_row) dispatching values to ColumnBuilder append methods, with helper functions (decimal_parts_to_i128, tds_date_to_arrow_date32, sql_time_to_micros, etc.) in mssql-arrow/src/query_reader.rs
- [X] T019 [US2] Implement finish() (drain builders into ArrayRef, construct RecordBatch), batch accumulation (row_count, is_batch_ready, end_row increment), and read_result_set() convenience method in mssql-arrow/src/query_reader.rs

**Checkpoint**: Query results with int, float, text, decimal, date, datetime, UUID columns produce correctly-typed RecordBatch. 250K rows at batch_size=10K yields 25 batches. All-null columns produce correctly-typed (not NullArray) results.

---

## Phase 5: User Story 3 — Stream Large Datasets in Chunks (Priority: P2)

**Goal**: Stream multiple RecordBatches into a single bulk insert session without accumulating entire dataset in memory

**Independent Test**: Stream 5 batches of 10K rows each, verify 50K total rows in destination table

### Implementation for User Story 3

- [X] T020 [US3] Implement write_batches() — iterate &[RecordBatch], validate consistent schema across batches, call serialize_batch() + write_to_server_zerocopy() per batch, track cumulative rows_affected, report rows-copied count on failure in mssql-arrow/src/bulk_copy.rs
- [X] T021 [P] [US3] Implement on_progress() callback — accept FnMut(BulkCopyProgress), fire at notification_interval row boundaries during write_batch and write_batches in mssql-arrow/src/bulk_copy.rs

**Checkpoint**: 5 batches × 10K rows stream into single table. Varying batch sizes (1K, 50K, 100, 20K) all insert correctly. Progress callback fires during operation.

---

## Phase 6: User Story 4 — Full SQL Server Type Support (Priority: P2)

**Goal**: Complete bidirectional type coverage for all supported SQL Server types including PLP encoding for MAX columns

**Independent Test**: Create table with one column per supported type, bulk insert Arrow batch, read back via query reader, verify round-trip correctness for every type

### Implementation for User Story 4

- [X] T022 [P] [US4] Implement serialize_bool (BITN: [0x01][val]/[0x00]) and serialize_u8 (INTN 1-byte) in mssql-arrow/src/serializer.rs
- [X] T023 [P] [US4] Implement serialize_i16 (INTN 2-byte) and serialize_f32 (FLTN 4-byte) in mssql-arrow/src/serializer.rs
- [X] T024 [P] [US4] Implement serialize_binary with u16 byte-count prefix and [0xFF][0xFF] null sentinel in mssql-arrow/src/serializer.rs
- [X] T025 [P] [US4] Implement serialize_date (Date32→DATEN 3-byte LE, epoch-to-days conversion) and serialize_time (Time64Microsecond→TIMEN scale-dependent bytes) in mssql-arrow/src/serializer.rs
- [X] T026 [P] [US4] Implement serialize_datetime2 (Timestamp→DATETIME2N time+date bytes) and serialize_datetimeoffset (Timestamp+tz→DTON time+date+offset bytes) in mssql-arrow/src/serializer.rs
- [X] T027 [P] [US4] Implement serialize_uuid — FixedSizeBinary(16) with TDS byte-swap for UNIQUEIDENTIFIER wire format in mssql-arrow/src/serializer.rs
- [X] T028 [P] [US4] Implement serialize_nvarchar_plp (PLP_UNKNOWN_LEN 8B + chunk_len 4B + UTF-16LE + PLP_TERMINATOR 4B, PLP_NULL 8B for null) and serialize_binary_plp in mssql-arrow/src/serializer.rs
- [X] T029 [US4] Add LargeUtf8, Utf8View, and Dictionary(_, Utf8)-encoded string dispatch — resolve dictionary indices to values, downcast to appropriate string array type in mssql-arrow/src/serializer.rs
- [X] T030 [US4] Add all remaining type mapping entries (Bool↔BIT, UInt8↔TINYINT, Int16↔SMALLINT, Float32↔REAL, Binary↔VARBINARY, Date32↔DATE, Time64↔TIME, Timestamp↔DATETIME2, Timestamp+tz↔DATETIMEOFFSET, FSB16↔UNIQUEIDENTIFIER, Utf8↔XML, Utf8↔JSON, read-only DATETIME/SMALLDATETIME/MONEY/SMALLMONEY) in mssql-arrow/src/type_mapping.rs
- [X] T031 [US4] Update serialize_batch() orchestrator to dispatch all new type serializers, branch on is_plp() for NVARCHAR/VARBINARY to select PLP vs standard serializer in mssql-arrow/src/serializer.rs

**Checkpoint**: All 20+ bidirectional type mappings round-trip correctly. PLP columns (NVARCHAR(MAX), VARBINARY(MAX)) serialize and insert correctly. LargeUtf8, Utf8View, Dictionary strings all write as NVARCHAR. Unsupported Arrow types produce descriptive errors.

---

## Phase 7: User Story 5 — Automatic Schema Matching (Priority: P3)

**Goal**: Auto-align Arrow columns to destination table by name; reject mismatches before data transfer

**Independent Test**: Insert with matching names (succeeds), mismatched count (errors), incompatible types (errors with details), fewer columns with nullable dest (succeeds with nulls)

### Implementation for User Story 5

- [X] T032 [US5] Implement automatic column matching by name — compare Arrow field names to destination column names (case-insensitive), build source→dest index mapping without explicit ColumnMapping in mssql-arrow/src/type_mapping.rs
- [X] T033 [US5] Implement pre-transfer schema validation — detect column count mismatch, type incompatibility, decimal precision overflow; produce ArrowError with column name and type details in mssql-arrow/src/type_mapping.rs
- [X] T034 [US5] Handle nullable unmapped destination columns — when Arrow has fewer columns than destination and unmatched columns are nullable, emit null wire bytes for unmapped columns in serialized rows in mssql-arrow/src/type_mapping.rs and mssql-arrow/src/serializer.rs

**Checkpoint**: Matching column names auto-match without explicit mappings. Type mismatches error before data transfer. Fewer Arrow columns succeed when dest allows nulls.

---

## Phase 8: Polish & Cross-Cutting Concerns

**Purpose**: Testing, validation, and cleanup across all stories

- [X] T035 [P] Add unit tests for serializer functions (null encoding, precision-dependent decimal lengths, UTF-16LE transcoding, PLP framing) in mssql-arrow/src/serializer.rs
- [X] T036 [P] Add unit tests for type mapping validation and column_metadata_to_data_type() in mssql-arrow/src/type_mapping.rs
- [X] T037 [P] Add unit tests for ColumnBuilder initialization and ArrowQueryReader schema inference in mssql-arrow/src/query_reader.rs
- [X] T038 Create bulk copy integration tests (5-col insert, null handling, column mapping, zero-row batch, cancellation) in mssql-arrow/tests/bulk_copy_integration.rs
- [X] T039 [P] Create query reader integration tests (diverse types, batch accumulation, null handling, multiple result sets) in mssql-arrow/tests/query_reader_integration.rs
- [X] T040 Create type round-trip integration tests for all 20+ supported type pairs in mssql-arrow/tests/type_roundtrip.rs
- [X] T041 Run cargo bfmt, cargo bclippy, cargo btest — verify zero warnings and ≥85% diff coverage

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — start immediately
- **Foundational (Phase 2)**: Depends on Phase 1 — BLOCKS all user stories
- **US1 (Phase 3)**: Depends on Phase 2 — delivers MVP
- **US2 (Phase 4)**: Depends on Phase 2 — can run in parallel with US1
- **US3 (Phase 5)**: Depends on US1 (write_batch)
- **US4 (Phase 6)**: Depends on US1 (serialize_batch orchestrator) and US2 (ColumnBuilder framework)
- **US5 (Phase 7)**: Depends on US1 (TypeMappingRegistry + write_batch)
- **Polish (Phase 8)**: Depends on all desired user stories being complete

### User Story Dependencies

- **US1 (P1)**: After Phase 2 — no cross-story dependencies
- **US2 (P1)**: After Phase 2 — independent of US1 (different files entirely)
- **US3 (P2)**: After US1 — extends ArrowBulkCopy with write_batches()
- **US4 (P2)**: After US1 + US2 — extends serializer and ColumnBuilder with remaining types
- **US5 (P3)**: After US1 — extends TypeMappingRegistry with auto-matching

### Within Each User Story

- Serializer functions before orchestrator (US1: T007–T010 → T011)
- Type mappings parallel with serializer orchestrator (US1: T012 ∥ T011)
- Builder before terminal operation (US1: T013 → T014)
- Mapping function before ColumnBuilder (US2: T015 → T016)
- ColumnBuilder before RowWriter impl (US2: T016 → T017 → T018 → T019)

---

## Parallel Opportunities

### After Phase 2 completes — US1 and US2 in parallel

```
Developer A (US1 write path):          Developer B (US2 read path):
T007 serialize_i32/i64  ─┐             T015 column_metadata_to_data_type
T008 serialize_f64       ├─ parallel    T016 ColumnBuilder enum
T009 serialize_nvarchar  │             T017 from_metadata()
T010 serialize_decimal  ─┘             T018 RowWriter impl (25 methods)
T011 serialize_batch()                 T019 finish() + read_result_set()
T012 type mappings (parallel w/ T011)
T013 ArrowBulkCopy builder
T014 write_batch()
```

### Within US4 — all serializer tasks parallel

```
T022 bool/u8   ─┐
T023 i16/f32    │
T024 binary     │
T025 date/time  ├─ all parallel (independent functions)
T026 dt2/dto    │
T027 uuid       │
T028 PLP        │
T029 string var ─┘
T030 type mappings (parallel with T022–T029)
T031 update serialize_batch() (depends on T022–T030)
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup (T001–T003)
2. Complete Phase 2: Foundational (T004–T006)
3. Complete Phase 3: User Story 1 (T007–T014)
4. **STOP and VALIDATE**: Bulk insert 100K rows with 5 columns, verify ≥300 Krows/s
5. Deploy/demo if ready

### Incremental Delivery

1. Setup + Foundational → crate compiles
2. US1 → bulk insert works for core types (MVP!)
3. US2 → query reader works, round-trip possible
4. US3 → streaming multi-batch inserts
5. US4 → all SQL Server types supported, PLP for MAX columns
6. US5 → auto schema matching for ergonomics
7. Polish → tests, coverage, lint compliance

---

## Notes

- All `.rs` files must start with `// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT License.`
- Use `pub(crate)` for internal types (ArrowSerializer, TypeMappingRegistry, ColumnBuilder, ArrowBulkLoadRow)
- Use `tracing` instrumentation (`#[instrument]`, `debug!`, `error!`) on public methods
- PLP detection: check `BulkCopyColumnMetadata.length_type.is_plp()` to branch serializer
- Zero per-row heap allocations for fixed-size types — pre-allocate row buffers, append in-place
- Commit after each task or logical group

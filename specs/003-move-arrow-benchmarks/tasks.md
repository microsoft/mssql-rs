# Tasks: Move Arrow Benchmarks to mssql-arrow Crate

**Input**: Design documents from `/specs/003-move-arrow-benchmarks/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, quickstart.md

**Tests**: Not explicitly requested — no test tasks generated.

**Organization**: Tasks grouped by user story to enable independent implementation and testing of each story.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (e.g., US1, US2, US3)
- Include exact file paths in descriptions

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Configure `mssql-arrow` crate for benchmarks and update shared test helpers

- [X] T001 Add `criterion = "0.5"` dev-dependency and `[[bench]] name = "arrow_tds_bench"` with `harness = false` to mssql-arrow/Cargo.toml
- [X] T002 Create `mssql-arrow/benches/` directory (placeholder file or via first bench file move)
- [X] T003 Add `get_scalar_value` helper function and required imports (`ColumnValues`, `ResultSet`, `ResultSetClient`) to mssql-arrow/tests/common/mod.rs

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: No foundational tasks — the mssql-arrow crate already has all source code, test infrastructure, and dependencies in place. Phase 1 setup is sufficient.

**Checkpoint**: Setup ready — user story implementation can begin

---

## Phase 3: User Story 1 — Run Criterion Microbenchmarks Locally (Priority: P1) 🎯 MVP

**Goal**: Move the Criterion serialization benchmark to mssql-arrow so `cargo bench -p mssql-arrow` produces throughput measurements for 3 serialization paths × 2 dataset sizes.

**Independent Test**: Run `cargo bench -p mssql-arrow` and verify 6 benchmark variants produce elements/sec throughput output.

### Implementation for User Story 1

- [X] T004 [US1] Copy mssql-tds/benches/arrow_tds_bench.rs to mssql-arrow/benches/arrow_tds_bench.rs and update import paths (keep `mssql_tds::` imports unchanged per research decision)
- [X] T005 [US1] Verify `cargo bench -p mssql-arrow` compiles and runs all 6 benchmark variants (3 paths × 2 sizes)

**Checkpoint**: `cargo bench -p mssql-arrow` succeeds with throughput output for arrow_direct, arrow_direct_preencoded, and arrow_via_column_values at 10K and 100K rows

---

## Phase 4: User Story 2 — Run End-to-End SQL Server Benchmark (Priority: P2)

**Goal**: Move the end-to-end bulk copy benchmark to mssql-arrow so it can be run against a live SQL Server to compare three serialization approaches.

**Independent Test**: Run `cargo nextest run -p mssql-arrow --run-ignored=only bench_arrow_bulk_copy` against a SQL Server instance and verify per-iteration throughput plus summary with speedup ratios.

### Implementation for User Story 2

- [X] T006 [US2] Copy mssql-tds/tests/bench_arrow_bulk_copy.rs to mssql-arrow/tests/bench_arrow_bulk_copy.rs and update `mod common` and import paths to use mssql-arrow's test infrastructure
- [X] T007 [US2] Verify the moved end-to-end benchmark compiles with `cargo nextest run -p mssql-arrow --no-run`

**Checkpoint**: End-to-end benchmark compiles in mssql-arrow. With a SQL Server instance, all 3 paths (pre-serialized, materialized, streaming) produce throughput data and row-count assertions pass.

---

## Phase 5: User Story 3 — Add ArrowBulkCopy Path to End-to-End Benchmark (Priority: P3)

**Goal**: Add a fourth benchmark path that uses the `mssql-arrow` crate's `ArrowBulkCopy` API, demonstrating the crate's high-level API achieves competitive throughput.

**Independent Test**: Run the end-to-end benchmark and confirm the ArrowBulkCopy path reports throughput alongside the three manual paths, with performance within 80% of the best manual path.

### Implementation for User Story 3

- [X] T008 [US3] Add Path D (ArrowBulkCopy) to mssql-arrow/tests/bench_arrow_bulk_copy.rs: create temp table, run iterations using `ArrowBulkCopy::execute`, measure timing
- [X] T009 [US3] Add ArrowBulkCopy row-count verification and include Path D in the summary statistics output (avg, warm, speedup) in mssql-arrow/tests/bench_arrow_bulk_copy.rs
- [X] T010 [US3] Update benchmark header print to include "D=ArrowBulkCopy" in mssql-arrow/tests/bench_arrow_bulk_copy.rs

**Checkpoint**: End-to-end benchmark runs 4 paths. ArrowBulkCopy path produces throughput metrics and is included in the summary comparison.

---

## Phase 6: Cleanup & Validation

**Purpose**: Remove originals from mssql-tds, clean up unused deps, final validation

- [X] T011 [P] Delete mssql-tds/benches/arrow_tds_bench.rs and remove its `[[bench]]` entry from mssql-tds/Cargo.toml
- [X] T012 [P] Delete mssql-tds/tests/bench_arrow_bulk_copy.rs
- [X] T013 Remove `arrow-buffer` from mssql-tds/Cargo.toml `[dev-dependencies]` (arrow-array and arrow-schema retained for `#[cfg(test)] arrow_writer` module)
- [X] T014 Run `cargo bclippy` to verify workspace compiles cleanly with no warnings
- [X] T015 Run `cargo bfmt` to verify formatting across workspace
- [X] T016 Verify `cargo bench -p mssql-arrow` still runs successfully after cleanup

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — can start immediately
- **User Story 1 (Phase 3)**: Depends on T001, T002 (Cargo.toml setup and benches dir)
- **User Story 2 (Phase 4)**: Depends on T003 (get_scalar_value in common/mod.rs)
- **User Story 3 (Phase 5)**: Depends on T006–T007 (end-to-end benchmark must be in place)
- **Cleanup (Phase 6)**: Depends on phases 3–5 completion — only delete originals after migration verified

### User Story Dependencies

- **User Story 1 (P1)**: Independent — only needs Cargo.toml setup
- **User Story 2 (P2)**: Independent — only needs common/mod.rs update
- **User Story 3 (P3)**: Depends on User Story 2 (adds to the end-to-end benchmark file moved in US2)

### Parallel Opportunities

- T001, T002, T003 can all run in parallel (different files)
- T004 can start as soon as T001+T002 complete; T006 can start as soon as T003 completes
- T011, T012 can run in parallel (different files being deleted)

---

## Parallel Example: User Story 1 + 2 Setup

```text
# These three setup tasks can all run in parallel:
T001: Update mssql-arrow/Cargo.toml (criterion + bench config)
T002: Create mssql-arrow/benches/ directory
T003: Update mssql-arrow/tests/common/mod.rs (get_scalar_value)

# Then US1 and US2 implementation can proceed in parallel:
T004: Move Criterion bench (US1) — depends on T001+T002
T006: Move end-to-end bench (US2) — depends on T003
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: T001–T003 (Setup)
2. Complete Phase 3: T004–T005 (Criterion bench move)
3. **VALIDATE**: `cargo bench -p mssql-arrow` produces 6 throughput measurements
4. This alone delivers the core benchmark capability

### Incremental Delivery

1. Setup (T001–T003) → Infrastructure ready
2. User Story 1 (T004–T005) → Local benchmarks work → **MVP complete**
3. User Story 2 (T006–T007) → End-to-end benchmarks work
4. User Story 3 (T008–T010) → ArrowBulkCopy comparison added
5. Cleanup (T011–T016) → Originals removed, workspace clean

---

## Notes

- All moved files retain the `// Copyright (c) Microsoft Corporation.` / `// Licensed under the MIT License.` header
- Import paths stay as `mssql_tds::...` per research decision — no re-exports through mssql-arrow
- Criterion `criterion = "0.5"` must remain in mssql-tds/Cargo.toml (used by `perf.rs` bench) — only remove the arrow-specific `[[bench]]` entry
- The `async-trait` crate is already a regular dependency of mssql-arrow, so no additional dev-dep needed for the end-to-end benchmark's `#[async_trait]` usage

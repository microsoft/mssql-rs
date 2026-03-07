# Research: Move Arrow Benchmarks to mssql-arrow Crate

**Feature**: 003-move-arrow-benchmarks
**Date**: 2025-07-15

## 1. Criterion [[bench]] Configuration

**Decision**: Add Criterion 0.5 as a dev-dependency and a `[[bench]]` entry with `harness = false` to `mssql-arrow/Cargo.toml`.

**Rationale**: This is the standard Criterion setup. Criterion 0.5 requires Rust ≥1.70; workspace uses 1.90, so no compatibility concerns. Arrow dependencies (`arrow-array`, `arrow-schema`, `arrow-buffer`) are already regular `[dependencies]` in mssql-arrow, so they're available to benches without additional dev-dep entries.

**Alternatives considered**: Using `divan` or built-in `#[bench]` — rejected because the existing benchmark already uses Criterion, and the mssql-tds crate already has Criterion for its `perf.rs` bench. Consistency wins.

**Required Cargo.toml additions**:
```toml
[[bench]]
name = "arrow_tds_bench"
harness = false

[dev-dependencies]
criterion = "0.5"
```

## 2. Import Path Strategy

**Decision**: Keep all imports as `mssql_tds::...` — do not re-export through mssql-arrow.

**Rationale**: The benchmarks directly test TDS-level serialization internals (BulkCopy, BulkLoadRow, StreamingBulkLoadWriter, DecimalParts, SqlString). These are TDS protocol types, not Arrow abstraction types. The existing mssql-arrow integration tests already use `mssql_tds::` paths for the same types (e.g., `bulk_copy_integration.rs` uses `mssql_tds::datatypes::column_values::ColumnValues`). Re-exporting would bloat mssql-arrow's public API for no user benefit.

**Alternatives considered**: Re-export common types through mssql-arrow — rejected to avoid coupling the benchmark's internal TDS usage to mssql-arrow's public API surface.

## 3. mssql-tds Cleanup After Migration

**Decision**: Remove `[[bench]] name = "arrow_tds_bench"` entry and `arrow-array`, `arrow-schema`, `arrow-buffer` from mssql-tds dev-dependencies. Keep `criterion = "0.5"` (still needed by `perf.rs` bench).

**Rationale**: Grep across all `mssql-tds/tests/` and `mssql-tds/benches/` confirms no other file imports arrow crates. The `perf.rs` benchmark still requires Criterion. Removing unused dev-deps reduces compile times and dependency surface.

**Files affected**:
- `mssql-tds/benches/arrow_tds_bench.rs` — delete
- `mssql-tds/tests/bench_arrow_bulk_copy.rs` — delete
- `mssql-tds/Cargo.toml` — remove `[[bench]]` entry and 3 arrow dev-deps

## 4. get_scalar_value Migration

**Decision**: Copy `get_scalar_value` to `mssql-arrow/tests/common/mod.rs` and add the required imports (`ColumnValues`, `ResultSet`, `ResultSetClient`).

**Rationale**: The end-to-end benchmark calls `common::get_scalar_value` three times for row-count verification. The function uses let-chains (`if let ... && let ...`), which is stable in Rust 1.87+ and supported by edition 2024.

**Required import additions to mssql-arrow's common/mod.rs**:
```rust
use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
use mssql_tds::datatypes::column_values::ColumnValues;
```

## 5. mod common Pattern

**Decision**: Use `mod common;` at the top of the moved benchmark file, matching the existing pattern in mssql-arrow's integration tests.

**Rationale**: Both `mssql-tds` and `mssql-arrow` integration tests declare `mod common;` at the file top level. The `common/mod.rs` directory structure is already set up in `mssql-arrow/tests/common/mod.rs`. No changes needed to the module declaration pattern.

## 6. async-trait Dev-Dependency

**Decision**: Add `async-trait = "0.1.89"` to mssql-arrow dev-dependencies if needed by the moved benchmark. However, `async-trait` is already in mssql-arrow's regular `[dependencies]`, so it's available to tests and benches without a separate dev-dep entry.

**Rationale**: The end-to-end benchmark uses `#[async_trait]` for `BulkLoadRow` implementations. Since `async-trait` is a regular dependency of mssql-arrow, no additional dev-dep is needed.

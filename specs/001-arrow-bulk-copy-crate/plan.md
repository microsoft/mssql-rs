# Implementation Plan: Arrow Bulk Copy Crate (`mssql-arrow`)

**Branch**: `001-arrow-bulk-copy-crate` | **Date**: 2026-03-07 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/001-arrow-bulk-copy-crate/spec.md`

## Summary

Create a new `mssql-arrow` Rust crate that provides high-performance bulk insert of Arrow `RecordBatch` data to SQL Server and reading query results into Arrow `RecordBatch` format. The write path uses columnar iteration with pre-serialization to TDS wire format (proven at 363 Krows/s, 2.09× vs intermediate-value path), including PLP encoding for MAX-type columns. The read path uses upfront metadata-based schema inference via `ColumnMetadata` to produce deterministic Arrow schemas. The crate keeps an independent copy of the Arrow row writer (not a dev-dependency on mssql-tds); future goal is to remove all Arrow dependencies from mssql-tds. Integrates exclusively through mssql-tds public API: `BulkCopy`/`BulkLoadRow` for writes, `ResultSet`/`RowWriter` for reads.

## Technical Context

**Language/Version**: Rust edition 2024  
**Primary Dependencies**: `mssql-tds` (workspace path dep), `arrow-array 57`, `arrow-schema 57`, `arrow-buffer 57`  
**Storage**: N/A (wire protocol to SQL Server)  
**Testing**: `cargo nextest` with `cargo-llvm-cov`, 85% diff coverage target; integration tests require SQL Server via `.env`  
**Target Platform**: Linux, Windows, macOS (same as mssql-tds)  
**Project Type**: Library crate (workspace member)  
**Performance Goals**: ≥300 Krows/s bulk insert (5-col dataset), ≥1.5× throughput vs ColumnValues path  
**Constraints**: Zero per-row heap allocations for fixed-size types; operates through mssql-tds public API only; no dev-dependency relationship back to mssql-tds  
**Scale/Scope**: ~2,500 LOC estimated; 6 source modules, 20+ type mappings bidirectional, PLP encoding for MAX types

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

Constitution is not yet ratified (template placeholders only). No gates to enforce. Project conventions from `.github/copilot-instructions.md`:

- [x] Copyright headers on all `.rs` files
- [x] `thiserror` for error types, `TdsResult<T>` type alias
- [x] Async via Tokio + `async-trait`
- [x] `pub(crate)` for internal APIs
- [x] `tracing` crate for instrumentation
- [x] Clippy with `-D warnings`
- [x] No `cargo test` — use `cargo btest` / `cargo nextest`
- [x] Integration tests in `tests/` directory, unit tests inline

Post-design re-check: all conventions satisfied. PLP support and upfront schema inference add complexity but don't violate any conventions.

## Project Structure

### Documentation (this feature)

```text
specs/001-arrow-bulk-copy-crate/
├── plan.md              # This file
├── research.md          # Phase 0: resolved unknowns & design decisions
├── data-model.md        # Phase 1: entity definitions & type mappings
├── quickstart.md        # Phase 1: usage examples
├── contracts/           # Phase 1: public API contracts
│   └── api.md           # Public Rust API surface
└── tasks.md             # Phase 2 output (/speckit.tasks command)
```

### Source Code (repository root)

```text
mssql-arrow/
├── Cargo.toml
├── src/
│   ├── lib.rs               # Crate root, public re-exports
│   ├── bulk_copy.rs          # ArrowBulkCopy builder, ArrowBulkLoadRow (BulkLoadRow impl)
│   ├── query_reader.rs       # ArrowQueryReader (RowWriter impl), upfront schema inference
│   ├── serializer.rs         # Per-type Arrow→TDS wire serialization, null encoding, PLP framing
│   ├── type_mapping.rs       # Arrow↔SqlDbType mapping registry, ColumnMetadata→DataType, validation
│   └── error.rs              # ArrowError, conversions to TdsResult
└── tests/
    ├── bulk_copy_integration.rs    # End-to-end bulk insert tests (requires SQL Server)
    ├── query_reader_integration.rs # End-to-end query→Arrow tests
    └── type_roundtrip.rs           # Per-type write+read verification
```

**Structure Decision**: New workspace member crate alongside existing crates. Independent from mssql-tds (no dev-dependency relationship). Follows established pattern: `src/` for library, `tests/` for integration tests, `#[cfg(test)]` for unit tests. Added to `members` in root `Cargo.toml`.

## Complexity Tracking

No constitution violations to justify. The crate boundary is the simplest architecture that keeps Arrow dependencies isolated from core TDS consumers.

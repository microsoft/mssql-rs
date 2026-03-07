# Implementation Plan: Move Arrow Benchmarks to mssql-arrow Crate

**Branch**: `003-move-arrow-benchmarks` | **Date**: 2025-07-15 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/003-move-arrow-benchmarks/spec.md`

## Summary

Migrate two Arrow benchmark files from the `mssql-tds` crate to `mssql-arrow`: a Criterion microbenchmark (`arrow_tds_bench.rs`) measuring serialization throughput locally, and an end-to-end SQL Server benchmark (`bench_arrow_bulk_copy.rs`) measuring real bulk copy throughput across three approaches. Add a fourth benchmark path using the `mssql-arrow` crate's `ArrowBulkCopy` API. Remove originals from `mssql-tds` after migration.

## Technical Context

**Language/Version**: Rust 1.90 (edition 2024)
**Primary Dependencies**: arrow-array 57, arrow-schema 57, arrow-buffer 57, criterion 0.5, mssql-tds (path dep), async-trait 0.1.89
**Storage**: N/A (benchmark data is in-memory; end-to-end test uses SQL Server temp tables)
**Testing**: cargo nextest (end-to-end), cargo bench / Criterion 0.5 (microbenchmarks)
**Target Platform**: Linux (primary), cross-platform
**Project Type**: Library benchmarks (dev-only artifacts within the mssql-arrow crate)
**Performance Goals**: ≥300 Krows/s for 5-column dataset (pre-serialized path); ArrowBulkCopy path within 80% of best manual path
**Constraints**: Criterion benchmarks must run without network; end-to-end tests gated by `#[ignore]`
**Scale/Scope**: 2 files moved (~1,035 lines total), 1 new benchmark path added, Cargo.toml updates

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

The constitution file contains only template placeholders — no project-specific principles or constraints are defined. All gates pass trivially. The `copilot-instructions.md` serves as the effective governance document for code conventions; this plan complies with all its requirements:

- [x] File headers: All new/moved files will have the MIT copyright header
- [x] Naming: No new public types introduced (benchmarks are dev-only)
- [x] Testing: Using cargo nextest for end-to-end, Criterion for microbenchmarks
- [x] No AI slop: Benchmark code is performance-critical, no filler comments
- [x] Edition 2024: Consistent with workspace
- [x] Clippy clean: `-D warnings` enforced

## Project Structure

### Documentation (this feature)

```text
specs/003-move-arrow-benchmarks/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
└── checklists/
    └── requirements.md  # Spec quality checklist
```

### Source Code (repository root)

```text
mssql-arrow/
├── Cargo.toml           # Updated: +criterion dev-dep, +[[bench]] section
├── benches/
│   └── arrow_tds_bench.rs   # Moved from mssql-tds/benches/
├── src/                     # Existing (unchanged)
│   ├── lib.rs
│   ├── bulk_copy.rs
│   ├── error.rs
│   ├── query_reader.rs
│   ├── serializer.rs
│   └── type_mapping.rs
└── tests/
    ├── common/
    │   └── mod.rs           # Updated: +get_scalar_value helper
    ├── bench_arrow_bulk_copy.rs  # Moved from mssql-tds/tests/
    ├── bulk_copy_integration.rs
    ├── query_reader_integration.rs
    └── type_roundtrip.rs

mssql-tds/
├── Cargo.toml           # Updated: remove [[bench]] arrow_tds_bench, remove arrow dev-deps if unused
├── benches/
│   └── (arrow_tds_bench.rs removed)
└── tests/
    └── (bench_arrow_bulk_copy.rs removed)
```

**Structure Decision**: Benchmarks belong in the crate they measure. The Criterion bench goes in `mssql-arrow/benches/` with a `[[bench]]` entry. The end-to-end test goes in `mssql-arrow/tests/` alongside existing integration tests. No new directories beyond `benches/`.

## Complexity Tracking

No constitution violations to justify — this is a straightforward file migration with one additive benchmark path.

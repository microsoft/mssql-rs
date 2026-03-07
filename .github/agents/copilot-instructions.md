# mssql-rs Development Guidelines

Auto-generated from all feature plans. Last updated: 2026-03-07

## Active Technologies
- Rust edition 2024 + `mssql-tds` (workspace path dep), `arrow-array 57`, `arrow-schema 57`, `arrow-buffer 57` (001-arrow-bulk-copy-crate)
- N/A (wire protocol to SQL Server) (001-arrow-bulk-copy-crate)
- Rust 1.90 (edition 2024) + arrow-array 57, arrow-schema 57, arrow-buffer 57, criterion 0.5, mssql-tds (path dep), async-trait 0.1.89 (003-move-arrow-benchmarks)
- N/A (benchmark data is in-memory; end-to-end test uses SQL Server temp tables) (003-move-arrow-benchmarks)

- Rust edition 2024 + `mssql-tds` (workspace), `arrow-array 57`, `arrow-schema 57`, `arrow-buffer 57` (001-arrow-bulk-copy-crate)

## Project Structure

```text
src/
tests/
```

## Commands

cargo test [ONLY COMMANDS FOR ACTIVE TECHNOLOGIES][ONLY COMMANDS FOR ACTIVE TECHNOLOGIES] cargo clippy

## Code Style

Rust edition 2024: Follow standard conventions

## Recent Changes
- 003-move-arrow-benchmarks: Added Rust 1.90 (edition 2024) + arrow-array 57, arrow-schema 57, arrow-buffer 57, criterion 0.5, mssql-tds (path dep), async-trait 0.1.89
- 001-arrow-bulk-copy-crate: Added Rust edition 2024 + `mssql-tds` (workspace path dep), `arrow-array 57`, `arrow-schema 57`, `arrow-buffer 57`

- 001-arrow-bulk-copy-crate: Added Rust edition 2024 + `mssql-tds` (workspace), `arrow-array 57`, `arrow-schema 57`, `arrow-buffer 57`

<!-- MANUAL ADDITIONS START -->
<!-- MANUAL ADDITIONS END -->

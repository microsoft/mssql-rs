# Copilot Instructions — mssql-tds

## Project Overview

Rust implementation of the TDS (Tabular Data Stream) protocol for SQL Server. Cargo workspace with multi-language FFI bindings.

| Crate | Purpose | Edition |
|---|---|---|
| `mssql-tds` | Core TDS protocol library | 2024 |
| `mssql-js` | Node.js bindings (NAPI-RS) | 2024 |
| `mssql-tds-cli` | Interactive CLI client | 2024 |
| `mssql-mock-tds` | Mock TDS server for testing | 2024 |
| `mssql-py-core` | Python bindings (PyO3/maturin) — **excluded from workspace** | 2021 |
| `mssql-odbc` | ODBC driver (msodbcsql18) | 2024 |

## Build / Test / Lint

### Pre-Push Checklist (Windows)

```powershell
cargo bfmt              # format check (workspace only)
cargo bclippy            # lint — warnings are errors
cargo btest              # test with coverage via nextest
```

Or use the scripts that also cover `mssql-py-core`:

```powershell
.\scripts\bfmt.ps1      # fmt check: workspace + mssql-py-core
.\scripts\bclippy.ps1   # clippy: workspace + mssql-py-core
```

### Cargo Aliases (defined in `.cargo/config.toml`)

- `cargo bfmt` → `cargo fmt -- --check`
- `cargo bclippy` → `cargo clippy --workspace --frozen --all-features --all-targets -- -D warnings`
- `cargo btest` → `cargo llvm-cov nextest --workspace --frozen --no-report --all-targets --no-fail-fast --profile ci --success-output immediate`

### JS (mssql-js)

```powershell
cd mssql-js
yarn install
yarn build               # NAPI-RS native addon
yarn test                # tsc + AVA
yarn lint                # ESLint, zero warnings
yarn format:check        # Prettier
```

### Test Infrastructure

- **Test runner:** `cargo nextest` (not `cargo test`)
- **Coverage:** `cargo-llvm-cov`, 85% diff coverage target in CI
- **CI profile:** `.config/nextest.toml` — 5 retries, JUnit XML output
- **Mock server:** `mssql-mock-tds` crate for unit/integration tests without a live SQL Server
- **Integration tests** require a `.env` file with connection details (uses `dotenv` crate)
- **Rust tests:** Unit tests in `#[cfg(test)]` inline modules for pure logic; integration tests in `tests/` directory
- **Python tests:** Shared fixtures and env helpers in `conftest.py` — always reuse existing patterns from sibling test files
- **Kerberos tests:** Gated by `KERBEROS_TEST=1` env var; CI uses Docker-based Kerberos setup in `kerberos-test/`

## Code Conventions

### File Header

Every `.rs` file must start with:

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
```

### Naming

- `Tds` prefix for core public types: `TdsClient`, `TdsTransport`, `TdsResult`, `TdsConnectionProvider`, `TdsTokenStreamReader`
- Standard Rust conventions: `snake_case` functions, `PascalCase` types, `SCREAMING_SNAKE_CASE` constants

### Architecture Patterns

- **Layered:** Transport → IO (packet reader/writer) → Token stream → Message → Client API
- **Module organization:** File `foo.rs` declares `pub mod` items, implementations live in `foo/` subdirectory
- **Error handling:** `thiserror` derive macros, `TdsResult<T>` type alias in `core.rs`
- **Async:** Tokio runtime, `async-trait` for async trait methods
- **Visibility:** Use `pub(crate)` for internal APIs, deliberate public surface
- **Tracing:** `tracing` crate (`debug!`, `error!`, `info!`, `trace!`, `#[instrument]`)
- **Cancellation:** `CancelHandle` wrapping `tokio_util::CancellationToken`
- **Authentication:** Two-phase resolution (validate inputs → resolve method) in `connection/`. Kerberos/GSSAPI for integrated auth cross-platform.

### FFI Patterns

- **JS:** `#[napi]` attributes, `Arc<Mutex<TdsClient>>` for thread safety
- **Python:** `#[pymodule]`, `#[pyclass]` via PyO3
- **ODBC:** `#[unsafe(no_mangle)] pub extern "C"` functions, cdylib producing msodbcsql18 shared library

## Git Conventions

### Commits

- Imperative tense: "Add", "Fix", "Refactor" (not "Added", "Fixes")
- Short subject lines, no body unless needed
- This repo does **not** use conventional commit prefixes (`feat:`, `fix:`, etc.)

### Branches

- Feature branches: `dev/<developer>/<feature-name>`
- Integration branch: `development`
- Default branch: `main`

### Pre-Commit Hook

The hook at `dev/hooks/pre-commit` auto-runs `cargo fmt` on workspace + `mssql-py-core` and blocks the commit if formatting changes are detected. Install with:

```bash
./setup-hooks.sh
```

## Code Quality — No AI Slop

- No verbose comments restating what the code does — Rust is expressive enough
- No filler phrases: "This ensures that...", "In order to...", "It's worth noting..."
- No redundant validation or duplicate logic
- No multi-line explanations of obvious test steps
- No excessive blank lines or formatting noise
- Doc comments (`///`) should add value — explain *why*, not *what*
- Would a senior Rust engineer roll their eyes at this? Then don't write it.

## What NOT to Do

- Don't use `cargo test` directly — use `cargo btest` or `cargo nextest`
- Don't add dependencies without `--frozen` working (update `Cargo.lock` explicitly)
- Don't skip clippy — `-D warnings` means any warning is a build failure
- Don't forget `mssql-py-core` when running fmt/clippy — it's excluded from the workspace and needs separate runs
- Don't add `cfg(test)` modules for integration tests — use the `tests/` directory
- Don't invent new test patterns — follow existing fixtures, helpers, and env-loading conventions from `conftest.py` and sibling test files
- Don't add guards, rejections, or platform checks without grepping the codebase first — check CI configs, test infrastructure (`kerberos-test/`, `tests/`), and README for existing support. A spec saying "X is platform-specific" doesn't mean this codebase hasn't already solved it cross-platform.

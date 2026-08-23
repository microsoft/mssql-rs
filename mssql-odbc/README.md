# mssql-odbc

Rust implementation of the Microsoft ODBC Driver 18 for SQL Server (`msodbcsql18`),
built on top of [mssql-tds](../mssql-tds).

## What it does

Produces a shared library (`libmsodbcsql18.so` / `libmsodbcsql.18.dylib` / `msodbcsql18.dll`) that implements
the ODBC C API. The ODBC Driver Manager (`unixODBC` on Linux/macOS, `odbc32` on Windows)
loads it via `dlopen` — applications use standard ODBC calls without knowing the driver
is written in Rust.

## Build

```bash
cargo build              # debug build
cargo build --release    # release build
```

Output location: `target/{debug,release}/` with a platform-specific filename:

| Platform | Output file |
|---|---|
| Linux | `libmsodbcsql18.so` |
| macOS | `libmsodbcsql18.dylib` |
| Windows | `msodbcsql18.dll` |

The `build.rs` script embeds platform-specific metadata:
- **Linux:** `soname` → `libmsodbcsql-18.4.so.1.1`
- **macOS:** `install_name` → `libmsodbcsql.18.dylib`
- **Windows:** no extra linker args needed

## Testing

### Rust unit tests

```bash
cargo btest -p mssql-odbc
```

### C++ e2e tests (Google Test)

End-to-end tests that exercise the driver through the ODBC Driver Manager,
matching the msodbcsql gtest infrastructure. See [tests/e2e/README.md](tests/e2e/README.md).

```bash
cd tests/e2e
./run_e2e.sh               # builds driver + registers + cmake + ctest
```

Or run test binaries directly (self-registers the driver automatically):

```bash
./build/smoke_test
./build/alloc_env_test
```

## Tracing

Tracing is disabled by default. Enable it with environment variables:

| Variable | Default | Description |
|---|---|---|
| `MSSQL_TDS_TRACE` | `false` | Set to `true` to enable tracing output |
| `MSSQL_TDS_TRACE_LEVEL` | `warn` | Tracing filter expression (`tracing_subscriber::EnvFilter`) |

Examples:

```bash
# Enable default warn-level logging
MSSQL_TDS_TRACE=true cargo btest -p mssql-odbc

# ODBC-driver-focused debug logs only
MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_LEVEL="warn,msodbcsql18=debug" cargo btest -p mssql-odbc

# Full filter syntax is supported
MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_LEVEL="warn,msodbcsql18=debug,mssql_tds=off" cargo btest -p mssql-odbc
```

## Architecture

```
Application
    ↓ ODBC C API (SQLAllocHandle, SQLDriverConnect, ...)
Driver Manager (unixODBC / odbc32)
    ↓ dlopen / LoadLibrary
libmsodbcsql18.so (this crate)
    ↓
mssql-tds (TDS protocol)
    ↓
SQL Server
```

Each ODBC entry point is a thin `pub unsafe extern "C"` wrapper in `exports.rs`
that the Driver Manager resolves by symbol name. The wrapper delegates to a
layered impl: panic boundary (`ffi_entry!` macro) → unsafe shim (raw-pointer
validation) → safe core (business logic). See the conventions file below for
details.

## Conventions

Before writing or modifying code in this crate, read
[`.github/instructions/mssql-odbc.instructions.md`](../.github/instructions/mssql-odbc.instructions.md).
It covers panic safety, FFI boundary conventions (the mandatory `ffi_entry!`
macro and safe-core/unsafe-shell split), unsafe-code rules, memory ownership
rules, concurrency and handle-hierarchy locking, diagnostic posting
(`post_sql_error` vs. `post_tds_error`), and testing requirements (the
`TestHandles` helper).

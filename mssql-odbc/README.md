# mssql-odbc

Rust implementation of the Microsoft ODBC Driver 18 for SQL Server (`msodbcsql18`),
built on top of [mssql-tds](../mssql-tds).

## What it does

Ships a shared library (`mssqlodbc.so` / `mssqlodbc.dylib` / `mssqlodbc.dll`) that implements
the ODBC C API. The ODBC Driver Manager (`unixODBC` on Linux/macOS, `odbc32` on Windows)
loads it via `dlopen` — applications use standard ODBC calls without knowing the driver
is written in Rust.

## Build

```bash
cargo build
bash scripts/finalize-artifact.sh debug

cargo build --release
bash scripts/finalize-artifact.sh release
```

On Windows, run `scripts/finalize-artifact.ps1` with `-BuildProfile debug` or
`-BuildProfile release` after the corresponding Cargo build. Cargo uses the internal
target name `mssqlodbc`; on Linux and macOS the finalization scripts create the
shipped artifact, while on Windows Cargo already emits it and the script resolves
its path.

Output location: `target/{debug,release}/` with a platform-specific filename:

| Platform | Output file |
|---|---|
| Linux | `mssqlodbc.so` |
| macOS | `mssqlodbc.dylib` |
| Windows | `mssqlodbc.dll` |

The `build.rs` script embeds platform-specific metadata:
- **Linux:** `soname` → `mssqlodbc.so`
- **macOS:** `install_name` → `mssqlodbc.dylib`
- **Windows:** no extra linker args needed

## Testing

### Rust unit tests

```bash
cargo btest -p mssqlodbc
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
| `MSSQL_TDS_TRACE_DIR` | unset | When tracing is enabled, non-empty directory for a per-process trace file; when unset, tracing uses stderr |

Examples:

```bash
# Enable default warn-level logging
MSSQL_TDS_TRACE=true cargo btest -p mssqlodbc

# ODBC-driver-focused debug logs only
MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_LEVEL="warn,mssqlodbc=debug" cargo btest -p mssqlodbc

# Full filter syntax is supported
MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_LEVEL="warn,mssqlodbc=debug,mssql_tds=off" cargo btest -p mssqlodbc

# Write to a timestamped file in an explicit directory
MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_DIR=/var/log/myapp cargo btest -p mssqlodbc

# Write to the current directory explicitly
MSSQL_TDS_TRACE=true MSSQL_TDS_TRACE_DIR=. cargo btest -p mssqlodbc
```

Trace filenames have the form `mssql_tds_trace_<timestamp>_<pid>.log`. Each event starts with an
RFC 3339 UTC timestamp, thread ID, level, target, and event fields. Multiline event values can
continue onto subsequent lines. General span fields are excluded because they can contain SQL text
and parameter values. Event fields may still contain sensitive data; configure a trusted directory
whose permissions are appropriate for it.

On Unix, trace files are created with mode `0600`. The driver warns for directories writable by
group or other users and when the directory is inside the system temporary directory. Relative
directories, including `.`, are resolved when the first ODBC call captures the configuration and
are unaffected by later changes to the host process's current directory. Configuration cannot be
changed while the driver remains loaded.

File tracing is intended for diagnostics. The driver keeps one synchronized file handle open while
an ODBC environment is live and closes it after the last environment is freed, before the host may
unload the driver. Events emitted without a live environment use transient handles. If another
environment is later allocated in the same process, the file is reopened lazily. Writes are
synchronous; the driver does not create a background logging thread. Trace files are not rotated or
removed automatically.

## Architecture

```
Application
    ↓ ODBC C API (SQLAllocHandle, SQLDriverConnect, ...)
Driver Manager (unixODBC / odbc32)
    ↓ dlopen / LoadLibrary
mssqlodbc.so (this crate)
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

## Connection busy gate

`SQLFetch`/`SQLFetchScroll`/`SQLGetData` release the connection's busy claim
(`DbcState::active_stmt`) as soon as the wire is drained for the current
statement, instead of holding it for the statement's whole cursor lifetime —
matching msodbcsql's wire-state `FIsBusyReadingData` gate (see AB#47508).
This costs a one-token read-ahead on ordinary fetches: no extra round trip,
but returning row N can now wait on row N+1's header arriving. See
`release_busy_if_row_exhausted` in `src/api/exec_common.rs` for the full
trade-off and why it was accepted as-is.

## Parameter array results

Prepared parameter arrays can return rows from `SELECT`, `INSERT ... OUTPUT`,
and procedures. Fetch the current result normally and use `SQLMoreResults` to
advance through the results in parameter-set order. Completion counts and
statuses are deferred until the corresponding sets finish; inspect the final
bookkeeping after navigation reaches `SQL_NO_DATA`. Closing the cursor drains
unread results without executing any parameter set again.

`SQLGetInfo(SQL_PARAM_ARRAY_SELECTS)` reports `SQL_PAS_BATCH`. Non-row-returning
arrays still complete during `SQLExecute` and report their aggregate row count.

## Conventions

Before writing or modifying code in this crate, read
[`.github/instructions/mssql-odbc.instructions.md`](../.github/instructions/mssql-odbc.instructions.md).
It covers panic safety, FFI boundary conventions (the mandatory `ffi_entry!`
macro and safe-core/unsafe-shell split), unsafe-code rules, memory ownership
rules, concurrency and handle-hierarchy locking, diagnostic posting
(`post_sql_error` vs. `post_tds_error`), and testing requirements (the
`TestHandles` helper).

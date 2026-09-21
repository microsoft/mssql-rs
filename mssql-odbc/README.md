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

### Buffer safety with Miri

The `miri-odbc` nextest profile selects the opt-in `memory_safety` unit-test
modules and the parameter reader's existing misalignment tests. They cover
unaligned values and indicators, initialized read extents, string terminators
and capacities, fixed-width writes, untouched error outputs, and reuse of caller
buffers. They also run as ordinary unit tests; no production code is replaced
under Miri. The UTF-16 reader cases check both aligned and byte-offset input,
preserving lossy decoding, explicit lengths, and NUL termination. Parameter
cases vary value, indicator, and length alignment independently, including
temporal inputs, ignored storage, and length sentinels. Column-wise arrays and
packed row-wise parameter bindings exercise production address calculations
with nonzero offsets and distinct first/last values.

The SQL NULL case with an uninitialized octet-length slot tests only
`read_indicator`'s early return, not full execution. Execution's earlier
data-at-execution probes still require readable, initialized non-null
octet-length slots for input and input/output parameters.

PR validation runs this profile under Miri on **Windows x64 and Linux x64**
only, using `nightly-2026-09-06` and seed 0. The Linux job uses the existing
Ubuntu build container. Test failures and empty selections fail the job, and
each platform publishes a separate ODBC Miri JUnit report. The ordinary native
test run still includes these tests; ARM64, macOS, and Alpine do not run Miri.
The `--package mssqlodbc` option scopes the run to the driver. The shared filter
uses only test names so it also parses in the smaller Kerberos workspace,
which omits ODBC.

The Build stage's `miriToolchain` variable in
`.pipeline/templates/validation-stages.yml` holds the CI pin; keep the local
commands below on the same version when updating it.

From the repository root, with `cargo-nextest` installed:

```powershell
cargo fetch
rustup toolchain install nightly-2026-09-06 --profile minimal --component miri,rust-src
cargo +nightly-2026-09-06 miri nextest run --frozen -p mssqlodbc --lib --profile miri-odbc
```

`cargo fetch` creates the local, gitignored lockfile and restores dependencies
before the frozen run. On Windows, a long checkout path can make Cargo use a
compiler response file, which this Miri version does not support. In that case,
set a short, dedicated build directory before running Miri:

```powershell
$env:CARGO_TARGET_DIR = Join-Path $env:TEMP "odbc-miri"
```

Run the same selection natively with:

```powershell
cargo nextest run --frozen -p mssqlodbc --lib --profile miri-odbc
```

The tests keep unread input tails uninitialized so Miri can detect over-reads
that stay inside an allocation. Output sentinels additionally catch writes
outside the declared slot even when those writes remain inside the backing
allocation. Deliberate misalignment is asserted before the call, and C struct
padding is not assumed to be initialized.

This profile intentionally excludes handle fixtures (which start an I/O-enabled
Tokio runtime), socket-based mock servers, native authentication/TLS, and the
C++ Driver Manager tests. It does not test Windows DLL unloading or replace
native end-to-end tests or fuzzing. Keep Miri's alignment and aliasing checks
enabled; a passing run covers only the inputs and executions exercised.

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

## Bound fetch performance

Bound fetches borrow their per-fetch descriptor snapshot rather than copying a
binding for every cell. SQL type resolution is deferred until a binding requests
`SQL_C_DEFAULT`, and complete inline rows need no PLP metadata snapshot.
After a packet-boundary continuation, resident columns return to synchronous
decoding; network waits retain the existing cancellation and timeout handling.
Datetimeoffset conversion uses checked 64-bit arithmetic while preserving the
out-of-range rejection of the wider calculation.
Same-encoding wide delivery in bound fetches and `SQLGetData` copies complete
UTF-16 code units without decoding them. This preserves unpaired surrogates,
BOM-like units, and embedded NULs for the application's decoding policy.
Actual encoding conversions use the explicit encoding without BOM sniffing:
leading BOM-shaped bytes remain data, including in non-Unicode `varchar` values.
The materialized-value fast paths require an even byte length before
using the raw-unit copy helper. Odd-length PLP streams retain their existing
behavior and are not covered by this parity claim.
Bound buffers trim a real surrogate pair if truncation would split it, but keep
already-unpaired units. `SQLGetData` can split a pair across calls because the
caller retrieves the remaining units on its next call.

The native `GetDataUtf16Test` and `FetchScrollUtf16Test` cases reproduce these
fetch behaviors against SQL Server through the Driver Manager, comparing raw
units rather than decoded strings. `BoundTruncationPreservesOnlyCompletePairs`
checks real-server truncation; the Rust
`bound_wide_plp_preserves_units_across_wire_chunks` test additionally uses a mock
server to force a surrogate pair across a PLP chunk boundary that a SQL query
cannot control.

## Parameter array results

Prepared parameter arrays can return rows from `SELECT`, `INSERT ... OUTPUT`,
and procedures. Fetch the current result normally and use `SQLMoreResults` to
advance through the results in parameter-set order. Completion counts and
statuses are deferred until the corresponding sets finish; inspect the final
bookkeeping after navigation reaches `SQL_NO_DATA`. Closing the cursor drains
unread results without executing any parameter set again.

`SQLGetInfo(SQL_PARAM_ARRAY_SELECTS)` reports `SQL_PAS_BATCH`. Non-row-returning
arrays still complete during `SQLExecute` and report their aggregate row count.

RPC value encoding avoids per-parameter boxed futures. Complete buffered
DONE-family and RETURNSTATUS tokens are decoded without constructing the
asynchronous parser; cancellation still uses the normal ATTENTION settlement
path, and incomplete tokens retain the existing network-read behavior.
Optional streaming declarations and encryption metadata are stored out of line
so ordinary parameter arrays do not copy their unused storage for every value.
Small RPC headers and type metadata are written together when buffered space
allows; packet-boundary writes retain normal overflow and cancellation handling.
Response-token reads skip clock sampling for unlimited query timeouts while
retaining elapsed-time accounting for finite and exhausted budgets.
Inlining hints target parameter positioning, conversion, RPC encoding, and
response/value dispatch. The large conversion and serialization functions use
`#[inline]`, leaving the final inlining decision to the compiler.

## Conventions

Before writing or modifying code in this crate, read
[`.github/instructions/mssql-odbc.instructions.md`](../.github/instructions/mssql-odbc.instructions.md).
It covers panic safety, FFI boundary conventions (the mandatory `ffi_entry!`
macro and safe-core/unsafe-shell split), unsafe-code rules, memory ownership
rules, concurrency and handle-hierarchy locking, diagnostic posting
(`post_sql_error` vs. `post_tds_error`), and testing requirements (the
`TestHandles` helper).

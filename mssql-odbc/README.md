# mssql-odbc

`mssql-odbc` is a cross-platform ODBC 3.x driver for Microsoft SQL Server and
Azure SQL, written in Rust and built on [mssql-tds](../mssql-tds). It exposes
the native ODBC C API as a shared library that applications load through the
platform's ODBC Driver Manager.

> [!IMPORTANT]
> The driver is alpha software under active development. It is being validated
> as an opt-in backend for
> [mssql-python](https://github.com/microsoft/mssql-python), but it is not yet a
> complete, general-purpose replacement for Microsoft ODBC Driver 18 for SQL
> Server.

## Distribution

The driver is included in
[`mssql-python-rs`](https://pypi.org/project/mssql-python-rs/0.1.0/), the native
runtime used by `mssql-python`. Version `0.1.0` is the latest release on PyPI,
with prebuilt wheels for supported Windows, macOS, and Linux targets.

This crate is not published independently to crates.io. Build it from this
workspace when developing or testing the ODBC library directly.

## How it works

```mermaid
flowchart TD
    application[Application]
    driverManager[ODBC Driver Manager<br/>Windows or unixODBC]
    odbcDriver[mssql-odbc<br/>Native shared library]
    tdsClient[mssql-tds<br/>TDS protocol client]
    sqlServer[SQL Server or Azure SQL]

    application -->|ODBC C API| driverManager
    driverManager -->|Loads driver| odbcDriver
    odbcDriver -->|Rust API| tdsClient
    tdsClient -->|TDS protocol| sqlServer
```

ODBC entry points cross a panic boundary, validate raw pointers in a small
unsafe layer, and delegate to safe Rust implementations. The driver targets
the behavior of Microsoft ODBC Driver 18 where practical while documenting and
testing deliberate differences.

Design notes for individual subsystems live beside the code: the parameter
binding and array-execution model in [`docs/parameters_plan.md`](docs/parameters_plan.md),
the fetch path in [`docs/typed-columnar-fetch-plan.md`](docs/typed-columnar-fetch-plan.md),
and the deliberate departures from msodbcsql in
[`docs/parity-deviations.md`](docs/parity-deviations.md).

## Platform artifacts

| Platform | Driver Manager | Library |
|---|---|---|
| Windows | Windows ODBC Driver Manager | `mssqlodbc.dll` |
| macOS | unixODBC | `mssqlodbc.dylib` |
| Linux | unixODBC | `mssqlodbc.so` |

## Build from source

Install the repository's pinned Rust toolchain. Linux and macOS builds also
need unixODBC development headers; the C++ end-to-end tests additionally need
CMake and a C++17 compiler.

From the repository root:

```bash
cargo build -p mssqlodbc
bash mssql-odbc/scripts/finalize-artifact.sh debug
```

For an optimized build, add `--release` to `cargo build` and pass `release` to
the finalization script.

On Windows, use PowerShell after building:

```powershell
cargo build -p mssqlodbc
./mssql-odbc/scripts/finalize-artifact.ps1 -BuildProfile debug
```

The finalization script prints the artifact path under the workspace Cargo
target directory.

## Test

Run the Rust test suite from the repository root:

```bash
cargo nextest run -p mssqlodbc --lib
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

### C++ end-to-end tests

The C++ end-to-end suite loads the built library through the real ODBC Driver
Manager and exercises the exported API:

```bash
bash mssql-odbc/tests/e2e/run_e2e.sh
```

On Windows, run:

```powershell
./mssql-odbc/tests/e2e/run_e2e.ps1
```

See the [end-to-end test guide](tests/e2e/README.md) for prerequisites,
connection configuration, targeted runs, coverage, and comparison testing
against `msodbcsql18`.

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

## Tracing

Tracing is disabled by default and is intended for diagnostics.

| Variable | Default | Purpose |
|---|---|---|
| `MSSQL_TDS_TRACE` | `false` | Set to `true` to enable tracing |
| `MSSQL_TDS_TRACE_LEVEL` | `warn` | Set a `tracing_subscriber::EnvFilter` expression |
| `MSSQL_TDS_TRACE_DIR` | unset | Write per-process trace files to this directory instead of stderr |

For example:

```bash
MSSQL_TDS_TRACE=true \
MSSQL_TDS_TRACE_LEVEL="warn,mssqlodbc=debug" \
MSSQL_TDS_TRACE_DIR="./traces" \
cargo nextest run -p mssqlodbc --lib
```

Trace events can contain sensitive data. Use a trusted directory with
appropriate permissions. Trace files are not rotated or deleted automatically.

## Contributing

Start with the repository [contribution guide](../CONTRIBUTING.md). Changes to
this crate must also follow the
[ODBC driver engineering guidelines](../.github/instructions/mssql-odbc.instructions.md),
which define the FFI safety, error handling, concurrency, parity, and testing
requirements.

Report security vulnerabilities according to the repository
[security policy](../SECURITY.md).

## License

Licensed under the [MIT License](../LICENSE).

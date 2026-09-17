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

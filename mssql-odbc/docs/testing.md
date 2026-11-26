# ODBC driver development tests

Start with the [README](../README.md#test) for Rust unit tests and the
[end-to-end guide](../tests/e2e/README.md) for Driver Manager tests, server
configuration, and reference-driver comparisons. Run the following commands
from the repository root.

## Application-thread stack budget

The application-thread stack regression also runs in debug/coverage builds with
a larger 512 KiB budget for unoptimized frames. To enforce the 128 KiB release
stack budget, run:

```bash
cargo nextest run --release --frozen -p mssqlodbc --lib -E 'test(exec_direct_on_small_stack)'
```

## JSON TVP acceptance

Run the JSON/TVP Rust regressions with:

```powershell
cargo nextest run -p mssqlodbc --frozen --lib -E 'test(tvp) | test(json)'
```

The production RPC serializer tests compare JSON TVP metadata, UTF-16LE
values, NULLs, and buffered DAE against `nvarchar(max)` bytes. They include
client-code-page input and multi-packet values.

The C++ `json_tvp_test` target adapts JSON TVP cases from
`SqlClientDrivers/msodbcsql`, branch `feature/JsonFeature`, revision
`88c4dd572852d225448b61173fe56fd59ab22e4c`:
`json_tvp_edge_test.cpp` and `json_outputparam_test.cpp` under
`/testsrc/ntdbms/sqlncli/ODBC/gtests/tests/`. It uses SQL Server's
`CAST(text AS json)` as the value oracle and covers bindings, DAE, metadata,
large values, invalid encoding, and statement reuse.

Reference source: `sqlccmd.cpp`, `PutParamData`, maps a JSON TVP column to
`SQL_WVARCHAR` with unlimited precision and converts binary input as UTF-8;
`sqlcfunc.cpp` rejects odd JSON WCHAR byte lengths. These are source-level
contracts, not measured retail parity. Run both driver legs against a
JSON-enabled server and a JSON-enabled native build, recording
`SQL_DRIVER_VER`; compiled tests or capability skips do not establish parity.
The native defaulted-column case requires the generic TVP
`SQL_CA_SS_COLUMN_HAS_DEFAULT_VALUE` descriptor field, which is outside this
JSON slice and is not covered here.

## Buffer safety with Miri

The `miri-odbc` nextest profile selects the opt-in `memory_safety` unit-test
modules and misalignment tests. It exercises ODBC caller-buffer handling,
including unaligned values and indicators, string bounds, fixed-width writes,
and column-wise and row-wise array address calculations. PR validation runs
this profile on Windows x64 and Linux x64.

With `cargo-nextest` installed:

```powershell
cargo fetch
rustup toolchain install nightly-2026-09-06 --profile minimal --component miri,rust-src
cargo +nightly-2026-09-06 miri nextest run --frozen -p mssqlodbc --lib --profile miri-odbc
```

The CI toolchain pin is `miriToolchain` in
[validation-stages.yml](../../.pipeline/templates/validation-stages.yml);
use that value if it changes.
On Windows, set a short target directory if Miri reports that compiler response
files are unsupported:

```powershell
$env:CARGO_TARGET_DIR = Join-Path $env:TEMP "odbc-miri"
```

Run the same selection natively with:

```powershell
cargo nextest run --frozen -p mssqlodbc --lib --profile miri-odbc
```

This profile intentionally excludes handle fixtures (which start an I/O-enabled
Tokio runtime), socket-based mock servers, native authentication/TLS, and the
C++ Driver Manager tests. It complements rather than replaces native
end-to-end tests and fuzzing.

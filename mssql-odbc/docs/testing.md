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

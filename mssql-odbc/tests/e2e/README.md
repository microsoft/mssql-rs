# ODBC Driver Google Tests

End-to-end tests for the mssql-odbc (Rust) ODBC driver,
built with [Google Test](https://github.com/google/googletest).

This test infrastructure mirrors the gtest layout used by the C++ msodbcsql driver
so tests can be migrated between the two.

## Prerequisites

| Requirement | Windows | Linux / macOS |
|---|---|---|
| **C++17 compiler** | Visual Studio 2022 (17.x) | GCC 7+ or Clang 5+ |
| **CMake** | Ships with VS 2022, or install separately (3.15+) | `sudo apt install cmake` / `brew install cmake` |
| **ODBC headers** | Included with Windows SDK | `sudo apt install unixodbc-dev` / `brew install unixodbc` |
| **Rust toolchain** | Required to build the driver | Same |

> Google Test is fetched automatically by CMake via FetchContent.

## Directory Layout

```
tests/e2e/
├── CMakeLists.txt              # Top-level CMake build
├── include/
│   └── odbc_test_fixture.h     # Base test fixture & assertion macros
├── lib/
│   ├── odbc_test_fixture.cpp   # ODBCTest fixture (HENV/HDBC/HSTMT lifecycle)
│   ├── odbc_test_utils.cpp     # Diagnostic helpers
│   └── odbc_test_config.cpp    # Environment-variable based config
├── tests/
│   ├── smoke_test.cpp          # Smoke tests (alloc, connect, query)
│   └── alloc_env_test.cpp      # SQLAllocHandle(ENV) variations
├── third_party/                # Reserved for git submodule (unused — using FetchContent)
├── run_e2e.sh                  # Build + test runner (Linux / macOS)
├── build_e2e.sh                # Build-only half for CI artifact reuse (Linux / macOS)
├── run_e2e.ps1                 # Build + test runner (Windows, requires admin)
└── README.md                   # This file
```

## Quick Start

### Linux / macOS

```bash
# From mssql-odbc/tests/e2e/
./run_e2e.sh

# Verbose CTest output + Rust tracing
./run_e2e.sh --verbose
```

In `--verbose` mode, `run_e2e.sh` defaults to:

- `MSSQL_TDS_TRACE=true`
- `MSSQL_TDS_TRACE_LEVEL=warn,msodbcsql18=debug`

unless those variables are already set in your environment.

To override the verbose default filter:

```bash
MSSQL_TDS_TRACE_LEVEL="warn,msodbcsql18=trace" ./run_e2e.sh --verbose
```

### Comparing against msodbcsql 18

`run_e2e.sh` can rerun the same gtest suite against the Microsoft C++ driver
and print a parity table — useful for spotting behavioral divergence between
the two drivers.

```bash
# Default ini path: /etc/odbcinst.ini
./run_e2e.sh --compare-with-msodbcsql

# Custom ini path
./run_e2e.sh --compare-with-msodbcsql --msodbcsql-ini=/opt/msodbcsql/odbcinst.ini
```

The two drivers register under **different** names, so both can be installed
side by side:

| Leg | Driver name (`ODBC_TEST_DRIVER`) |
|---|---|
| `mssql-odbc` | `ODBC Driver 18 for SQL Server (Rust)` |
| `msodbcsql` | `ODBC Driver 18 for SQL Server` |

Each leg exports `ODBC_TEST_DRIVER` so the same test binaries connect through
the right driver. Setting `ODBC_TEST_CONNSTR` overrides the whole connection
string and would pin both legs to one driver, so comparison mode rejects it.

The script exits `0` only if **both** runs pass *and* every test reaches the
same verdict in both legs. A divergence, a shared failure, or a test that ran
in only one leg fails the run:

```
Summary: 15 parity, 1 divergence(s), 0 shared failure(s), 0 skipped
=== Parity check FAILED (mssql-odbc rc=0, msodbcsql rc=0, parity rc=1) ===
```

CI runs this comparison on the Linux x64 PR build, which owns a SQL Server in
docker. `.pipeline/scripts/containerized-odbc-e2e.sh` installs a pinned
`msodbcsql18` from `packages.microsoft.com` when `ODBC_E2E_COMPARE=1`.

### Failure modes that are never silently green

Both runners abort — locally and in CI — when:

- `cargo build` or either `cmake` invocation exits non-zero.
- A ctest leg executes zero tests. ctest exits `0` and prints
  `No tests were found!!!` in that case, which previously turned a broken CMake
  configure into a passing run reporting `0 parity`.
- Any test diverges between the two legs (comparison mode).

In CI the scripts additionally dump `CMakeOutput.log`, `CMakeError.log`,
`LastTest.log`, and the discovered test executables, since there is no working
copy left to inspect afterwards.

### Collecting coverage

`run_e2e.sh --coverage` builds the Rust driver with LLVM source-based
instrumentation so the driver code exercised by the C++ tests — which load the
`.so` through the unixODBC Driver Manager as separate processes — is measured.
It writes a Cobertura report for `mssql-tds` + `mssql-odbc` (the cdylib
statically links `mssql-tds`, so both are covered).

```bash
# Report to the default path: <repo>/target/cobertura-odbc-e2e.xml
./run_e2e.sh --coverage

# Custom output path
./run_e2e.sh --coverage=/tmp/odbc-e2e.xml
```

This uses the same mechanism as `dev/test-python.sh --coverage`: everything runs
through `cargo llvm-cov` (`show-env` to instrument the build, `report` to emit
the report), so the LLVM version that reads the `.profraw` always matches the
rustc that produced the instrumented `.so`. In CI, the Linux x64 PR build sets
`ODBC_E2E_COVERAGE=1`, publishes the report as `CoberturaCoverageOdbcE2E_Linux`,
and the Merge Coverage stage unions it into the diff-coverage report.

### Windows (requires Administrator)

```powershell
# From mssql-odbc\tests\e2e\
.\run_e2e.ps1
```

Like `run_e2e.sh`, it can rerun the suite against msodbcsql 18 and print a
parity table. The Rust driver registers under its own name, so no registry swap
happens between the two legs, and the same parity gate applies: any divergence
fails the run.
```powershell
# Use the installed "ODBC Driver 18 for SQL Server" registration as the reference
.\run_e2e.ps1 -CompareWithMsodbcsql

# Point at a specific reference driver
.\run_e2e.ps1 -CompareWithMsodbcsql -MsodbcsqlDll 'C:\path\to\msodbcsql18.dll'
```

Install the reference driver with:

```powershell
winget install --id Microsoft.msodbcsql.18 --version 18.6.2.1 --exact
```

CI runs this comparison on the Windows x64 PR build (the leg with a local SQL
Server), installing the same pinned version before the suite.

When `ODBC_TEST_SERVER` is unset, a dev SQL Server on `localhost:1433` is
auto-detected — the password is taken from `ODBC_TEST_PWD`, then `SQL_PASSWORD`,
then `SQL_PASSWORD=` in `mssql-tds\.env`, falling back to integrated auth. This
matches `run_e2e.sh`.

`run_e2e.ps1 -Coverage` builds the Rust driver with LLVM source-based
instrumentation so the driver code exercised by the C++ tests — which load the
DLL through the Windows Driver Manager as separate processes — is measured. It
writes a Cobertura report for `mssql-tds` + `mssql-odbc` (the cdylib statically
links `mssql-tds`, so both are covered).

```powershell
# Report to the default path: <repo>\target\cobertura-odbc-e2e.xml
.\run_e2e.ps1 -Coverage

# Custom output path
.\run_e2e.ps1 -Coverage -CoverageOutput 'C:\tmp\odbc-e2e.xml'
```

This uses the same mechanism as `run_e2e.sh --coverage`: everything runs through
`cargo llvm-cov` (`show-env` to instrument the build, `report` to emit the
report), so the LLVM version that reads the `.profraw` always matches the rustc
that produced the instrumented DLL. In CI, the Windows x64 PR build runs the
suite with `-Coverage`, publishes the report as `CoberturaCoverageOdbcE2E_Windows`,
and the Merge Coverage stage unions it into the diff-coverage report.

Both scripts:
1. Build the Rust cdylib (`cargo build` from `mssql-odbc/`)
2. Register the driver with the platform's ODBC Driver Manager
3. Configure and build the gtest executables via CMake
4. Run all tests via CTest
5. Clean up the driver registration on exit (even on failure)

## Driver Registration

The test fixture does **not** register the driver — that is handled externally
by the run scripts or by manual setup. This matches how the C++ msodbcsql LTM
infrastructure works (`runtests.c`).

### How the scripts register the driver

- **Linux / macOS (`run_e2e.sh`)**: Creates a temp directory with an
  `odbcinst.ini` file registering the Rust driver as
  `[ODBC Driver 18 for SQL Server (Rust)]`, and sets `ODBCSYSINI` to point at
  it. The env var is scoped to the script process, so the parent shell is never
  affected. A `trap cleanup EXIT` ensures the temp directory is removed even on
  failure.

- **Windows (`run_e2e.ps1`)**: Writes `Driver` and `Setup` values under
  `HKLM\Software\ODBC\ODBCINST.INI\ODBC Driver 18 for SQL Server (Rust)`, so an
  installed msodbcsql18 registration is left untouched. Any pre-existing values
  under that key are saved beforehand and restored in a `try/finally` block.
  Only `-MsodbcsqlDll` temporarily repoints the reference registration, and it
  too is restored on exit.

### Manual registration (without the scripts)

If you prefer not to use the scripts, register the driver yourself. You can use
either name — the canonical `ODBC Driver 18 for SQL Server` (the default when
`ODBC_TEST_DRIVER` is unset), or a distinct name such as
`ODBC Driver 18 for SQL Server (Rust)` if you want it installed alongside
msodbcsql18. With a distinct name, set `ODBC_TEST_DRIVER` to match before
running the tests.

- **Linux / macOS**: Either add an entry to `/etc/odbcinst.ini`, or create
  your own `odbcinst.ini` in any directory and set `ODBCSYSINI` env var to that
  directory before running the tests.

- **Windows**: Add the following registry values (requires Administrator),
  substituting your chosen driver name for `<driver name>`:
  ```
  HKLM\Software\ODBC\ODBCINST.INI\<driver name>
      Driver = <path to msodbcsql18.dll>
      Setup  = <path to msodbcsql18.dll>

  HKLM\Software\ODBC\ODBCINST.INI\ODBC Drivers
      <driver name> = Installed
  ```

## Manual Build

### Linux

Register the driver first (see [Driver Registration](#driver-registration)),
then:

```bash
cd mssql-odbc && cargo build
cd tests/e2e
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug
cmake --build build -j$(nproc)
cd build && ctest --output-on-failure
```

### Windows (VS 2022)

Register the driver first (see [Driver Registration](#driver-registration)),
then:

```cmd
cd mssql-odbc && cargo build
cd tests\e2e
cmake -S . -B build -G "Visual Studio 17 2022" -A x64 -DODBC_E2E_FORCE_UNICODE=ON
cmake --build build --config Debug
cd build && ctest --output-on-failure -C Debug
```

## Running Connected Tests

Tests that require a live SQL Server **fail** when no connection is configured,
so an unconfigured environment is surfaced instead of silently passing. Set
environment variables to enable them:

### Auto-detection

When `ODBC_TEST_SERVER` is not set, `run_e2e.sh` probes `localhost:1433`. If a
SQL Server is listening, it auto-configures `ODBC_TEST_SERVER=localhost`,
`ODBC_TEST_UID=sa`, and resolves the password from `ODBC_TEST_PWD`,
`SQL_PASSWORD`, or `mssql-tds/.env` (in that order).

To bring up a local SQL Server in Docker:

```bash
./dev/dev-launchsql.sh
```

### Manual configuration

| Variable | Required? | Default | Description |
|---|---|---|---|
| `ODBC_TEST_SERVER` | Yes (for connected tests) | *(none)* | SQL Server hostname or `host,port` |
| `ODBC_TEST_UID` | Yes (for SQL auth) | *(none)* | SQL login username (e.g. `sa`) |
| `ODBC_TEST_PWD` | Yes (for SQL auth) | *(none)* | SQL login password |
| `ODBC_TEST_DATABASE` | No | `tempdb` | Database to connect to |
| `ODBC_TEST_DRIVER` | No | `ODBC Driver 18 for SQL Server` | ODBC driver name (the run scripts set this per leg) |
| `ODBC_TEST_DSN` | No | *(none)* | Pre-configured DSN (overrides server/driver) |
| `ODBC_TEST_CONNSTR` | No | *(none)* | Full connection string (overrides all above) |
| `ODBC_TEST_TRUST_CERT` | No | `Yes` | Trust server certificate (`Yes`/`No`) |

## Writing a New Test

### 1. Create the test source file

```cpp
// tests/my_feature_test.cpp
#include "odbc_test_fixture.h"

TEST_F(ODBCTest, MyFeatureWorks) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);
    // ... test logic ...
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}
```

### 2. Register it in CMakeLists.txt

```cmake
add_odbc_test(my_feature_test  tests/my_feature_test.cpp)
```

### 3. Build and run

```bash
cmake --build build && ctest --test-dir build --output-on-failure
```

## How It Works

Each test calls standard ODBC C APIs (`SQLAllocHandle`, `SQLDriverConnect`,
etc.) through the Driver Manager, which loads our shared library — the same
code path a real application uses.

## CI: prebuilt artifact flow (build once, test on many distros)

CI (the main-branch pipeline) does not rebuild the driver in every distro. It
splits the flow into a build half and a run half so a single set of binaries can
be exercised on many Linux versions:

- **`build_e2e.sh [--release] [--out=DIR]`** — builds the Rust driver and the
  C++ gtest binaries, then stages `build/` (with `libmsodbcsql18.so` copied
  inside) into `DIR`. That directory is published as a pipeline artifact.
- **`run_e2e.sh --skip-build [--driver=PATH]`** — skips all compilation. It
  restores the prebuilt `build/` tree, auto-resolves the driver from
  `build/libmsodbcsql18.so` (or `--driver`), registers it, and reruns the
  prebuilt binaries via CTest.

`CTestTestfile.cmake` bakes **absolute** paths to the test executables, so the
consumer must place `build/` back at the *same* absolute path it was built at.
In CI both the build and test jobs mount the repo at `/workspace`, so the paths
line up.

Binaries are libc/OpenSSL specific, so CI builds three tracks and reuses each
across matching distros:

| Track | Build base | Reused on |
|---|---|---|
| glibc modern (x64, arm64) | Ubuntu 22.04 (glibc 2.35, OpenSSL 3) | Debian bookworm, Ubuntu 22.04/24.04, Azure Linux 3 |
| musl (x64, arm64) | Alpine 3.18 (musl, OpenSSL 3) | Alpine 3.18–3.21 |
| glibc 2.28 (x64) | manylinux_2_28 / AlmaLinux 8 (OpenSSL 1.1) | RHEL 8 / UBI 8 |

A glibc-2.35 binary may fail to load on older glibc (e.g. RHEL 8's 2.28), and an
OpenSSL 3 binary won't find `libssl.so.1.1`, which is why the glibc-2.28 track
exists as a separate build.

# ODBC Driver Google Tests

End-to-end tests for the mssql-odbc (Rust) ODBC driver,
built with [Google Test](https://github.com/google/googletest).

This test infrastructure mirrors the gtest layout in the C++ msodbcsql driver
(`testsrc/ntdbms/sqlncli/ODBC/gtests/`) so tests can be migrated between the two.

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

### Windows (requires Administrator)

```powershell
# From mssql-odbc\tests\e2e\
.\run_e2e.ps1
```

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
  `odbcinst.ini` file and sets `ODBCSYSINI` to point at it. The env var is
  scoped to the script process, so the parent shell is never affected. A
  `trap cleanup EXIT` ensures the temp directory is removed even on failure.

- **Windows (`run_e2e.ps1`)**: Writes `Driver` and `Setup` values under
  `HKLM\Software\ODBC\ODBCINST.INI\ODBC Driver 18 for SQL Server`. The
  original values are saved beforehand and restored in a `try/finally` block,
  so an existing production driver installation is not permanently overwritten.

### Manual registration (without the scripts)

If you prefer not to use the scripts, register the driver yourself:

- **Linux / macOS**: Either add an entry to `/etc/odbcinst.ini`, or create
  your own `odbcinst.ini` in any directory and set `ODBCSYSINI` env var to that
  directory before running the tests.

- **Windows**: Add the following registry values (requires Administrator):
  ```
  HKLM\Software\ODBC\ODBCINST.INI\ODBC Driver 18 for SQL Server
      Driver = <path to msodbcsql18.dll>
      Setup  = <path to msodbcsql18.dll>

  HKLM\Software\ODBC\ODBCINST.INI\ODBC Drivers
      ODBC Driver 18 for SQL Server = Installed
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
cmake -S . -B build -G "Visual Studio 17 2022" -A x64
cmake --build build --config Debug
cd build && ctest --output-on-failure -C Debug
```

## Running Connected Tests

Tests that require a live SQL Server are automatically **skipped** when no
connection is configured. Set environment variables to enable them:

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
| `ODBC_TEST_DRIVER` | No | `ODBC Driver 18 for SQL Server` | ODBC driver name |
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

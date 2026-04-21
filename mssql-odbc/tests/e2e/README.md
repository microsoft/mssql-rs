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
├── run_e2e.sh                  # One-command build + test runner
└── README.md                   # This file
```

## Quick Start

```bash
# From mssql-odbc/tests/e2e/
./run_e2e.sh
```

This script:
1. Builds the Rust cdylib (`cargo build` from `mssql-odbc/`)
2. Registers the driver via a temporary `odbcinst.ini` + `ODBCSYSINI`
3. Configures and builds the gtest executables via CMake
4. Runs all tests via CTest

## Manual Build

### Linux

```bash
cd mssql-odbc && cargo build
cd tests/e2e
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug
cmake --build build -j$(nproc)
cd build && ctest --output-on-failure
```

### Windows (VS 2022)

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

The `run_e2e.sh` script registers the built `libmsodbcsql18.so` as an ODBC
driver by writing a temporary `odbcinst.ini` and setting `ODBCSYSINI` to point
at it. This lets unixODBC's Driver Manager load our Rust driver without
system-wide installation.

Each test calls standard ODBC C APIs (`SQLAllocHandle`, `SQLDriverConnect`,
etc.) through the Driver Manager, which `dlopen`s our shared library — the
same code path a real C/C++ application uses.

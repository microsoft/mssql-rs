# mssql-odbc performance benchmarks

A [Google Benchmark](https://github.com/google/benchmark) suite that measures the
`mssql-odbc` driver through the ODBC Driver Manager and compares it against
Microsoft's `msodbcsql18`.

The benchmark binaries are driver-agnostic: they pick a driver purely from the
`Driver={...}` keyword in the connection string. The same binary therefore
produces both sides of the comparison, so the numbers are apples-to-apples —
same client code, same driver manager, same server, same session.

This implements "Layer 2 — mssql-odbc via ODBC DM" from
[`docs/perf-testing-plan.md`](../../../docs/perf-testing-plan.md).

## What is measured

| Binary | Cases |
| --- | --- |
| `connect_bench` | `SQLDriverConnect` + `SQLDisconnect` round trip, statement handle alloc/free |
| `exec_bench` | `SQLExecDirect`, prepare-once/execute-many, prepare+execute each time, parameterized execute |
| `fetch_bench` | Narrow and wide row fetch at 100 / 1 000 / 10 000 rows |
| `datatype_bench` | Per-type retrieval cost: int, bigint, decimal, float, varchar, nvarchar, datetime2, uniqueidentifier, varchar(max) |

Row-producing cases build a session-scoped `#perf_rows` temp table once per
fixture, so the measured loop is the client-side fetch path rather than server
query planning.

## Prerequisites

- SQL Server reachable from this machine (a local instance is fine).
- `msodbcsql18` installed (it ships the driver manager on Windows and provides
  the reference numbers).
- CMake 3.20+, Ninja, and a C++17 toolchain.
  - **Windows:** Visual Studio 2022 with the "Desktop development with C++"
    workload. `run_perf.ps1` locates `vcvars64.bat` via `vswhere` and selects a
    toolset that actually ships the CRT import libraries — some installs have a
    newer toolset directory without them, which otherwise fails with
    `LNK1104: cannot open file 'MSVCRTD.lib'`.
  - **Linux/macOS:** `unixodbc-dev` (or `unixodbc` via Homebrew) for `sql.h`.
- The Rust driver registered with the driver manager (see below).

### One-time driver registration

The comparison selects drivers by name, so `mssql-odbc` needs its own driver
manager entry pointing at a path the developer can overwrite. This is the only
step that needs elevated rights, and it is done once.

**Windows** (elevated PowerShell):

```powershell
New-Item -Path 'C:\odbc-dev' -ItemType Directory -Force
$key = 'HKLM:\Software\ODBC\ODBCINST.INI\mssql-odbc dev'
New-Item -Path $key -Force | Out-Null
Set-ItemProperty -Path $key -Name 'Driver' -Value 'C:\odbc-dev\mssql-odbc-dev.dll'
Set-ItemProperty -Path $key -Name 'Setup'  -Value 'C:\odbc-dev\mssql-odbc-dev.dll'
Set-ItemProperty -Path 'HKLM:\Software\ODBC\ODBCINST.INI\ODBC Drivers' `
                 -Name 'mssql-odbc dev' -Value 'Installed'
```

`run_perf.ps1` then copies each fresh `target\release\msodbcsql18.dll` over
`C:\odbc-dev\mssql-odbc-dev.dll`, so no further registry writes are needed and
the benchmark run itself does not require Administrator.

**Linux/macOS**, add to `~/.odbcinst.ini`:

```ini
[mssql-odbc dev]
Description = mssql-odbc development build
Driver      = /home/<user>/.odbc-dev/libmssql_odbc.so
```

### Test database and login

Any login with permission to create session temp tables works. To create a
dedicated one:

```sql
CREATE DATABASE odbcperf;
GO
CREATE LOGIN odbcperf WITH PASSWORD = '<password>', CHECK_POLICY = OFF;
ALTER SERVER ROLE sysadmin ADD MEMBER odbcperf;
GO
```

Windows integrated authentication also works — omit `-Uid`/`-Pwd` and the
connection string uses `Trusted_Connection=Yes`.

## Running

```powershell
# Full comparison: builds the driver, builds the benches, runs both drivers.
.\run_perf.ps1 -Uid odbcperf -Pwd '<password>'

# One binary, longer measurement window.
.\run_perf.ps1 -Bench fetch_bench -MinTime 2.0 -Repetitions 5

# Re-run without rebuilding, restricted to a subset of cases.
.\run_perf.ps1 -SkipBuild -Filter 'BM_Fetch.*'

# Measure the Rust driver only.
.\run_perf.ps1 -RefDriver ''
```

```bash
./run_perf.sh --uid odbcperf --pwd '<password>'
./run_perf.sh --bench fetch_bench --min-time 2.0
```

Raw Google Benchmark JSON lands in `results/`, one file per binary per driver.
The runner prints a comparison of median real times:

```
Benchmark                  mssql-odbc   msodbcsql18   Ratio Verdict
---------                  ----------   -----------   ----- -------
BM_Connect_Disconnect      2.57 ms      11.01 ms       0.23 faster
BM_Fetch_NarrowRows/10000  17.34 ms     4.87 ms        3.56 slower
BM_Type_Decimal            ERROR        7.49 ms           - unsupported
```

`Ratio` is `mssql-odbc / msodbcsql18`, so below 1.0 means the Rust driver is
faster. A case the driver cannot service at all is reported as `unsupported`
rather than being dropped, so capability gaps stay visible next to the timings.

## Environment variables

The suite reuses the [`tests/e2e`](../e2e/README.md) contract, so a single
environment drives both suites. `run_perf.ps1` sets these for you.

| Variable | Meaning |
| --- | --- |
| `ODBC_TEST_DRIVER` | Driver name; this is what selects mssql-odbc vs msodbcsql18 |
| `ODBC_TEST_SERVER` | Server address |
| `ODBC_TEST_DATABASE` | Database name |
| `ODBC_TEST_UID` / `ODBC_TEST_PWD` | SQL login; omit both for integrated auth |
| `ODBC_TEST_DSN` | Connect by DSN instead of by driver name |
| `ODBC_TEST_CONNSTR` | Full connection string, overriding everything above |
| `ODBC_TEST_TRUST_CERT` | `TrustServerCertificate` value (default `Yes`) |
| `ODBC_TEST_ENCRYPT` | `Encrypt` value |

Running a binary directly is sometimes handy while iterating:

```powershell
$env:ODBC_TEST_DRIVER = 'mssql-odbc dev'
$env:ODBC_TEST_SERVER = 'localhost'
.\build\fetch_bench.exe --benchmark_filter=BM_Fetch_NarrowRows
```

## Building manually

```powershell
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release -DODBC_PERF_FORCE_UNICODE=ON
cmake --build build
```

`ODBC_PERF_FORCE_UNICODE` (default `ON` on Windows) compiles the benchmark
sources with `UNICODE`/`_UNICODE` so `SQLTCHAR` is `wchar_t`, matching how real
Windows ODBC applications are built. It is applied only to the benchmark
targets, not to the vendored Google Benchmark library.

## Interpreting results

- Benchmarks talk to a real server, so a share of every measurement is network
  and server time that both drivers pay equally. Ratios are more meaningful than
  absolute numbers, and `BM_AllocFree_Stmt` (no server round trip) is the
  cleanest signal of pure driver overhead.
- Run with `-Repetitions 3` or more and check the `_cv` rows in the raw output.
  A coefficient of variation above roughly 5% means the machine is too noisy to
  trust small differences.
- Only compare runs collected on the same machine against the same server.

## Driver capability notes

The benchmarks are written against what `mssql-odbc` currently exports, which
constrains the harness:

- `SQLBindCol` is not exported, so all retrieval goes through `SQLGetData`.
- `SQLGetData` supports `SQL_C_CHAR` and `SQL_C_WCHAR` only; the fixture reads
  every column as `SQL_C_CHAR`.
- `SQLBindParameter` supports `SQL_C_CHAR` bound to the `varchar` family only,
  so `BM_ParameterizedExecute` passes its parameter as text.

When these gaps close, the corresponding cases can be widened to also measure
bound-column fetch and native C-type conversion.

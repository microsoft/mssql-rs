# Standalone ODBC fetch benchmarks

This C++17 harness calls the platform ODBC Driver Manager (`odbc32` on Windows,
unixODBC on Linux). The selected registered driver is loaded by the Driver
Manager; the harness never links to the Rust driver.

## Prerequisites

- CMake 3.20+ and a C++17 compiler
- Windows SDK ODBC headers, or the unixODBC runtime and development package
- Git and network access while CMake fetches pinned Google Benchmark v1.9.1
- SQL Server 2022+ with database compatibility level 160 (`GENERATE_SERIES`)
- Permission to create and drop tables in the configured database

## Environment

| Variable | Default/fallback | Purpose |
|---|---|---|
| `ODBC_BENCH_DRIVER` | required | Registered ODBC driver name |
| `ODBC_BENCH_SERVER` | `SQL_SERVER`, then required | Server name/address |
| `ODBC_BENCH_DATABASE` | `tempdb` | Shared benchmark database |
| `ODBC_BENCH_UID` | `sa` | SQL login |
| `ODBC_BENCH_PWD` | `SQL_PASSWORD`, then required | SQL login password |
| `ODBC_BENCH_TRUST_CERT` | `Yes` | `TrustServerCertificate` value |
| `ODBC_BENCH_ENCRYPT` | `Mandatory` | `Encrypt` value |
| `ODBC_BENCH_PACKET_SIZE` | `32768` | TDS packet size, 512–32768 |
| `ODBC_BENCH_PACKET_SIZE_KEYWORD` | `PacketSize` | `PacketSize` or Microsoft driver spelling `Packet Size` |
| `ODBC_BENCH_SCENARIO` | all | Run only `narrow` or `wide` |

Missing required values, connection errors, absent/invalid tables, ODBC
warnings during retrieval, and correctness failures make the process fail.
There is no disconnected or skipped workload mode.

## Build

Linux:

```bash
cmake -S mssql-odbc-bench -B mssql-odbc-bench/build -DCMAKE_BUILD_TYPE=Release
cmake --build mssql-odbc-bench/build --parallel
```

Windows (Developer PowerShell):

```powershell
cmake -S mssql-odbc-bench -B mssql-odbc-bench\build -G "Visual Studio 17 2022" -A x64
cmake --build mssql-odbc-bench\build --config Release
```

## Set up, run, and clean up

Set the environment once, then create both permanent heaps:

```bash
./mssql-odbc-bench/build/mssql_odbc_bench_admin setup
```

```powershell
.\mssql-odbc-bench\build\Release\mssql_odbc_bench_admin.exe setup
```

Run a driver leg with standard Google Benchmark JSON output:

```bash
./mssql-odbc-bench/build/mssql_odbc_bench \
  --benchmark_repetitions=10 \
  --benchmark_out=candidate.json \
  --benchmark_out_format=json
```

```powershell
.\mssql-odbc-bench\build\Release\mssql_odbc_bench.exe `
  --benchmark_repetitions=10 `
  --benchmark_out=candidate.json `
  --benchmark_out_format=json
```

Change only `ODBC_BENCH_DRIVER` and the output path for other driver legs. Every
leg reads the same tables:

- `dbo.mssql_odbc_bench_narrow_2m_c15_mixed_fixed`
- `dbo.mssql_odbc_bench_wide_10k_c600_mixed_fixed`

After both legs:

```bash
./mssql-odbc-bench/build/mssql_odbc_bench_admin cleanup
```

```powershell
.\mssql-odbc-bench\build\Release\mssql_odbc_bench_admin.exe cleanup
```

## Workloads

- `fetch/narrow_2m_c15_mixed_fixed/rowset_1024`: 2,000,000 rows and one
  15-column pattern.
- `fetch/wide_10k_c600_mixed_fixed/rowset_1024`: 10,000 rows and 40 repeats of
  the same pattern.

The pattern is `BIT`, `TINYINT`, `SMALLINT`, `INT`, `BIGINT`, `REAL`,
`FLOAT(53)`, `DECIMAL(18,4)`, `DATE`, `TIME(7)`, `DATETIME2(7)`,
`DATETIMEOFFSET(7)`, `UNIQUEIDENTIFIER`, `CHAR(8)`, `NCHAR(8)`. Every column is
fixed-length and `NOT NULL`; the generated wide row is below SQL Server's
8,060-byte limit.

Each Google Benchmark sample is one full result-set retrieval
(`Iterations(1)`). `--benchmark_repetitions` controls the sample count.
Steady-clock timing starts immediately before `SQLExecDirect` and ends after
the final `SQLFetchScroll` returns `SQL_NO_DATA` and the row count is checked.
It includes `SQLNumResultCols`, every `SQLDescribeCol`, every `SQLBindCol`, and
all rowset fetches. Connection, buffer allocation, statement attributes,
setup, untimed full-data preflight, cursor close, and unbind are excluded.

JSON results include rows, cells, logical bytes, and execute,
metadata-plus-bind, and fetch phase counters, plus rows/s, cells/s, and logical
bytes/s. Preflight checks all indicators, row identity/coverage, an
order-independent checksum, and representative values for every generated type.

## Dedicated perf lab

`perf-lab/run-benchmarks.sh` and `perf-lab/run-benchmarks.ps1` build the harness
once, then measure three drivers side by side: the candidate, the mssql-odbc
commit in `perf-lab/baseline-commit.txt`, and Microsoft ODBC Driver 18 from the
version in `perf-lab/msodbcsql-version.txt`. Candidate and baseline order is
reversed between workloads, with Microsoft ODBC between them. The result
summary reports candidate changes relative to both comparison drivers; only
the candidate-versus-mssql-odbc-baseline result participates in the optional
regression gate. Raw Google Benchmark JSON and the median comparison are
written to the repository-level `results` directory.

The shared PerfTest template uploads `results/summary.md` to the Azure DevOps
Summary tab. It starts with the gate verdict and a three-median wall-time table,
then shows separate candidate-vs-baseline and candidate-vs-Microsoft diverging
bar tables. Green means lower wall time and red means higher wall time. One
square represents about five percentage points, capped at 20 squares so the
expected 60-95% ODBC differences stay readable. Every row also states the exact
wall-time change and comparator/candidate speedup factor. The runner echoes the
same report to the step log and retains raw JSON, context, text comparison, and
JSON comparison artifacts.

The perf-lab scripts default to five repetitions and report regressions without
failing the run while initial variance is established. Set
`ODBC_BENCH_REPETITIONS` to change the sample count and
`ODBC_BENCH_FAIL_ON_REGRESSION=1` to make the 10% comparison threshold a gate.

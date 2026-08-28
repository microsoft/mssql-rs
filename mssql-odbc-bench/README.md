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
the candidate-versus-mssql-odbc-baseline result participates in the regression
gate. Raw Google Benchmark JSON and every comparison artifact are written to
the repository-level `results` directory.

### Gate and confirmation

The default regression threshold is **5%** (`1.05`), matching the fixed-baseline
`mssql-tds` runner, and a confirmed regression **fails the run by default**.

The initial five-sample median is a screening pass, not the verdict. It selects
the candidate-vs-baseline regressions plus at most three of the largest apparent
improvements, and the runner then re-measures exactly those benchmarks with the
candidate and the pinned baseline back-to-back for **four confirmation rounds**.
The runners alternate which driver runs first to cancel stable position effects.
A result counts only when it reproduces in at least **3 of 4** rounds. The
headline wall-time change for a re-measured benchmark is the median of those
four confirmation ratios; the initial pass that triggered the re-measurement is
excluded so the outlier under test does not vote on its own verdict. Regression
hits are tracked independently from the initial direction, so an apparent
improvement that reverses into a 3-of-4 regression still fails the gate. Microsoft
ODBC stays informational and never gates.

The harness filters by scenario rather than by benchmark id, so the runner maps
each flagged benchmark back to its scenario and runs one process covering all
selected scenarios. Confirmation raw JSON, ratios, and per-round comparisons are
kept under `results/confirm/round<N>/`; the pre-confirmation comparison is kept
under `results/initial/`. Only the final report is written as `summary.md`.

### Comparison engine

Pairwise comparison is done by Google Benchmark's own `tools/compare.py` and
`gbench.report`, taken from the pinned v1.9.1 checkout in the CMake build tree,
so the tool and the harness stay on the same version. It is run twice — Rust
baseline vs candidate and Microsoft vs candidate — and both its text and JSON
outputs are retained. `.pipeline/scripts/compare-odbc-benchmarks.py` remains a
thin policy/report wrapper: exact benchmark-set validation, the combined
three-way report, custom throughput counters and pins, the gate, the
confirmation overrides, and the Azure DevOps Markdown.

The Mann-Whitney U test is disabled explicitly (`--no-utest`). Google Benchmark
documents nine repetitions as the minimum for a meaningful U test and the lab
runs five, so its p-values would be published without being trustworthy;
reproduction is established by the confirmation rounds instead. The official
Python code imports NumPy and SciPy at module scope even with `--no-utest`, so
both runners provision them into a private virtualenv under `target/` when the
host interpreter lacks them.

### Noise controls

Both runners capture a SQL Server configuration snapshot with sqlcmd (the shared
`mssql-tds-bench/perf-lab/sql-config-dump.sql`) to `results/sql-config.txt`, and
bracket the initial pass and every confirmation round with CPU
frequency/utilization samples in `results/cpu-telemetry.csv`. Both honor
`PERF_CLIENT_CPUS`/`BENCH_CPUS` for client CPU pinning.

Large-buffer allocation is tuned per platform because each retrieval allocates
bound rowset buffers for up to 600 columns by 1024 rows. Linux raises
`MALLOC_MMAP_THRESHOLD_` and disables heap trimming so those buffers are reused
instead of being re-mmapped and re-faulted every repetition. Windows has no
environment-level equivalent, so it keeps the child off the debug heap
(`_NO_DEBUG_HEAP`), raises the process priority class (inherited by every
benchmark leg), and requests the High performance power scheme; priority and
power scheme are restored when the run ends, as are process affinity and the
prior `_NO_DEBUG_HEAP` environment state.

Connection-churn network tuning is deliberately **not** applied. The
`mssql-tds` runner widens the ephemeral port range and enables TIME_WAIT reuse
for its `concurrent_connects` benchmark; this harness opens one connection in
`OdbcSession`, holds it for the whole process, and measures only statement
execution and fetching, so there is no port pressure to relieve.

### Report

The shared PerfTest template uploads `results/summary.md` to the Azure DevOps
Summary tab. It starts with the gate verdict and a three-median wall-time table,
then shows separate candidate-vs-baseline and candidate-vs-Microsoft diverging
bar tables, then an initial-vs-confirmed section listing what was flagged, how
many rounds reproduced it, and the confirmed change. Green means lower wall time
and red means higher wall time. One square represents about five percentage
points, capped at 20 squares so the expected 60-95% ODBC differences stay
readable. Every row also states the exact wall-time change and
comparator/candidate speedup factor; `⟳` marks a re-measured benchmark. The
runner echoes the same report to the step log.

### Perf-lab environment

| Variable | Default | Purpose |
|---|---|---|
| `ODBC_BENCH_REPETITIONS` | `5` | Samples per driver/workload |
| `ODBC_BENCH_REGRESSION_RATIO` | `1.05` | Regression threshold |
| `ODBC_BENCH_IMPROVEMENT_VERIFY_RATIO` | regression ratio | Threshold for verifying apparent wins |
| `ODBC_BENCH_IMPROVEMENT_VERIFY_MAX` | `3` | Cap on re-measured improvements |
| `ODBC_BENCH_CONFIRM_RUNS` | `4` | Confirmation rounds |
| `ODBC_BENCH_CONFIRM_QUORUM` | majority (`3`) | Rounds required to confirm |
| `ODBC_BENCH_FAIL_ON_REGRESSION` | `1` | Set `0` to publish the report without gating |

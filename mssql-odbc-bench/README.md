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
| `ODBC_BENCH_PACKET_SIZE` | `32767` | TDS packet size, 512–32767; set through both ODBC attribute and connection keyword, then read back |
| `ODBC_BENCH_PACKET_SIZE_KEYWORD` | `PacketSize` | `PacketSize` or Microsoft driver spelling `Packet Size` |
| `ODBC_BENCH_SCENARIO` | all | Run only `narrow`, `wide`, `rowset`, `varwidth`, or `getdata` |

Missing required values, connection errors, absent/invalid tables, ODBC
warnings during retrieval, and correctness failures make the process fail.
There is no disconnected or skipped workload mode. An unknown scenario is
rejected at startup rather than producing an empty result file.

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

Set the environment once, then create the benchmark tables:

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

- `dbo.mssql_odbc_bench_fixed_2m_c15`
- `dbo.mssql_odbc_bench_fixed_10k_c600`
- `dbo.mssql_odbc_bench_fixed_100k_c15`
- `dbo.mssql_odbc_bench_fixed_20k_c15`
- `dbo.mssql_odbc_bench_varwidth_100k_c7`
- `dbo.mssql_odbc_bench_lobmax_1k_c3`
- `dbo.mssql_odbc_bench_mixedlob_20k_c16`
- `dbo.mssql_odbc_bench_variant_20k_c4`

`mssql_odbc_bench_admin print-sql` prints the exact DDL, generator, and
projection SQL for the whole catalog without connecting, which is the offline way
to review or replay what setup sends.

After both legs:

```bash
./mssql-odbc-bench/build/mssql_odbc_bench_admin cleanup
```

```powershell
.\mssql-odbc-bench\build\Release\mssql_odbc_bench_admin.exe cleanup
```

## Workloads

Thirteen measurements over eight tables, all of them C++ calls into the driver.
Every id encodes the data shape, the row and column counts, and the access shape,
and is identical on all three driver legs. The shapes are drawn from what
`mssql-python` asks a driver to do; nothing here runs Python.

### `narrow` and `wide` — bound fetch at the fetchall cadence

- `fetch/narrow_2m_c15_fixed/bound_rowset_1000`: 2,000,000 rows, one 15-column
  pattern.
- `fetch/wide_10k_c600_fixed/bound_rowset_1000`: 10,000 rows, 40 repeats of the
  same pattern.

The pattern is `BIT`, `TINYINT`, `SMALLINT`, `INT`, `BIGINT`, `REAL`,
`FLOAT(53)`, `DECIMAL(18,4)`, `DATE`, `TIME(7)`, `DATETIME2(7)`,
`DATETIMEOFFSET(7)`, `UNIQUEIDENTIFIER`, `CHAR(8)`, `NCHAR(8)`. Every column is
fixed-length and `NOT NULL`; the generated wide row is below SQL Server's
8,060-byte limit.

### `rowset` — the rowset sizes a consumer actually asks for

The row-array sizes are 1, 64, and 1000 rather than a round 1024, because those
are the cadences `mssql-python` produces: `Cursor.arraysize` defaults to **1**, so
an unconfigured `fetchmany()` binds a one-row rowset; applications that raise it
land in the tens; and `fetchall()` caps its computed batch at **1000**
(`FetchAll_wrap` in `ddbc_bindings.cpp`).

- `fetch/rowset_100k_c15_fixed/bound_rowset_1`
- `fetch/rowset_100k_c15_fixed/bound_rowset_64`
- `fetch/rowset_100k_c15_fixed/bound_rowset_1000`

The same 100,000 rows in all three, so only the cadence differs.

`fetchmany()` does not bind once and drain: every call re-describes the columns,
rebinds them, sets the row-array size, fetches one rowset, resets the size to 1,
and unbinds. Two workloads model that complete lifecycle:

- `fetch/rowset_100k_c15_fixed/bind_cycle_rowset_64` — same table and cadence as
  `bound_rowset_64`, so the pair isolates the per-call bind/unbind cost.
- `fetch/rowset_20k_c15_fixed/bind_cycle_rowset_1` — the default `arraysize`, one
  full lifecycle per row. A smaller table because this shape issues dozens of
  ODBC calls per row.

### `varwidth` — nullable inline variable width

- `fetch/varwidth_100k_c7_nullable/bound_rowset_1000`
- `fetch/varwidth_100k_c7_nullable/bound_rowset_64`

An `INT` identity plus `VARCHAR(64|256|1024)` and `NVARCHAR(64|256|1024)`, all
nullable. Length varies per row (`1 + row_id % max`), and each column goes NULL
on a different one row in seven, so per-column indicators are checked rather than
assumed. Kept deliberately separate from `MAX`/PLP: inline variable width is a
bound-buffer path, PLP is not.

**No `VARBINARY` column.** `mssql-odbc` implements only the zero-length
`SQL_C_BINARY` length probe; delivering binary data into a real buffer is still
`HYC00` (AB#47239, see `mssql-odbc/docs/typed-columnar-fetch-plan.md`), and
binary-to-character hex is not implemented either. A `VARBINARY` column would
fail the candidate and baseline legs while the Microsoft leg passed, which is a
broken comparison rather than a measurement.

### `getdata` — row-at-a-time `SQLGetData`

A LOB or `sql_variant` column makes `mssql-python` abandon bound fetching for the
**whole** result and read every column of every row with `SQLGetData`, so these
four measure that path.

- `getdata/rowwise_20k_c15_fixed/inline_values` — ordinary fixed inline values
  read one cell at a time. Same columns as `bound_rowset_1`, so the pair
  separates the call shape from the cadence.
- `getdata/rowwise_1k_c3_lob_max/chunked_8192` — `NVARCHAR(MAX)` of 9,000-9,999
  characters and `VARCHAR(MAX)` of 20,000-20,999 bytes. Both are past one
  8192-byte chunk, so each value needs three continuation calls; preflight fails
  the run if a value arrives in fewer, because then the loop under test never ran.
- `getdata/rowwise_20k_c16_mixed_lob/whole_result_rowwise` — the 15-column fixed
  pattern plus one *small* `NVARCHAR(MAX)`. The payload is tiny on purpose: what
  this measures is one PLP column moving all sixteen onto the row-at-a-time path.
- `getdata/rowwise_20k_c4_variant/probe_colattribute` — `sql_variant` columns read
  through the exact sequence `mssql-python` uses: a zero-length `SQL_C_BINARY`
  probe, then `SQLColAttribute(SQL_CA_SS_VARIANT_TYPE)`, then the typed read. One
  variant column is nullable so the probe's `SQL_NULL_DATA` arm is exercised too.

  The probe passes a valid pointer with `BufferLength` 0 rather than `NULL`.
  `mssql-python` passes `NULL` and gets away with it because it `dlopen`s the
  driver and calls its exports directly; this harness goes through the Driver
  Manager, which rejects a `NULL` target with `HY009` before the driver sees the
  call. Both drivers treat the two forms identically.

  Only base types whose reported C type is unambiguous are used. `mssql-odbc`
  deliberately reports `SQL_C_CHAR` where msodbcsql reports `SQL_C_NUMERIC` for
  decimal/money variants, and a benchmark that depended on that difference would
  not be the same work on both drivers.

## Measurement boundaries

Each Google Benchmark sample is one full result-set retrieval
(`Iterations(1)`). `--benchmark_repetitions` controls the sample count.
Steady-clock timing starts immediately before `SQLExecDirect` and ends after the
result is exhausted and the row count is checked. What is inside depends on the
access shape:

| Access shape | Inside the timed region |
|---|---|
| `bound_rowset_*` | `SQLExecDirect`, `SQLNumResultCols`, every `SQLDescribeCol`, every `SQLBindCol`, all `SQLFetchScroll` calls |
| `bind_cycle_*` | the same, but describe + bind + row-array set + fetch + row-array reset + `SQLFreeStmt(SQL_UNBIND)` repeated once per rowset |
| `getdata/*` | `SQLExecDirect`, the initial describe, then per row: `SQLFetch`, and per column a `SQLDescribeCol` plus its `SQLGetData` calls (including LOB continuations and the variant probe/`SQLColAttribute` pair) |

The per-cell `SQLDescribeCol` in the row-at-a-time shape is not an accident: it
mirrors `SQLGetData_wrap`, which re-describes every column on every row, so it is
part of the cost the consumer actually pays.

Connection, buffer allocation, statement attributes set before execution, setup,
the untimed full-data preflight, cursor close, and the final unbind are all
outside the boundary. The `metadata_bind_ms` phase counter is therefore zero for
`bind_cycle_*`, where describe and bind are inside the fetch loop rather than
ahead of it; `fetch_ms` carries that work.

JSON results include rows, cells, logical bytes, `SQLGetData` call count, and
execute, metadata-plus-bind, and fetch phase counters, plus rows/s, cells/s, and
logical bytes/s. Logical bytes are the generator's own byte total, so all three
drivers are credited with identical payload regardless of representation.

Preflight checks all indicators, row identity/coverage, an order-independent
checksum, representative values for every generated type, the exact generated
length of every variable-width and `MAX` value, the LOB continuation count, and
the `sql_variant` probe/type/value sequence.

## Scope

Every measurement here is a C++ call into the mssql-odbc driver through the
platform ODBC Driver Manager. `mssql-python` is referenced only as a workload-shape
reference: it is where the rowset sizes, the bind/unbind cadence, and the
row-at-a-time `SQLGetData` sequences come from. Its own performance — the
interpreter, the pybind11 layer, and the Arrow conversion — is out of scope, and
no Python package is loaded inside any measurement.

The one Python in this lab is reporting tooling: Google Benchmark's `compare.py`
and `.pipeline/scripts/compare-odbc-benchmarks.py`, which run after the
measurements and never touch the driver.

```bash
python -m unittest discover .pipeline/scripts -p 'test_compare*.py'
```

## Dedicated perf lab

`perf-lab/run-benchmarks.sh` and `perf-lab/run-benchmarks.ps1` build the harness
once, then measure three drivers side by side: the candidate, the mssql-odbc
commit in `perf-lab/baseline-commit.txt`, and Microsoft ODBC Driver 18 from the
version in `perf-lab/msodbcsql-version.txt`. Each runs one leg per scenario per
driver and alternates candidate and baseline order per scenario, with Microsoft
ODBC between them. The result summary reports candidate changes relative to both
comparison drivers; only
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
each flagged benchmark back to its scenario and re-runs those scenarios only.
Confirmation raw JSON, ratios, and per-round comparisons are kept under
`results/confirm/round<N>/`; the pre-confirmation comparison is kept under
`results/initial/`. Only the final report is written as `summary.md`.

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
bound rowset buffers for up to 600 columns by 1000 rows. Linux raises
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
Summary tab. It starts by distinguishing the regression-gating Rust-baseline
comparison from the non-gating Microsoft-driver reference gap, then shows the
gate verdict and a three-median wall-time table,
then shows separate candidate-vs-baseline and candidate-vs-Microsoft diverging
bar tables, then an initial-vs-confirmed section listing what was flagged, how
many rounds reproduced it, and the confirmed change. Green means lower wall time
and red means higher wall time. One square represents about five percentage
points, capped at 20 squares so the expected 60-95% ODBC differences stay
readable. Every row also states the exact wall-time change and
comparator/candidate speedup factor; `⟳` marks a re-measured benchmark. The
Microsoft section explains that “informational” means it cannot fail this run,
not that the gap is insignificant. The runner echoes the same report to the step
log.

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

## Comparing with Tedious

`compare-tedious.js` compares the release-built `mssql-js` public `Request`
API with Tedious 20.0.0. Node.js 22 or newer is required.

From `mssql-js`, build the native addon and JavaScript API:

```bash
yarn install --immutable
yarn build --cargo-flags="--frozen"
yarn buildapi
cd perf
npm ci
```

Start a disposable SQL Server container. The harness creates the
`mssql_js_benchmark` database and replaces its `dbo.driver_bench_rows` and
`dbo.driver_bench_lob` tables; do not point it at a production server.

```bash
export SQL_PASSWORD="$(openssl rand -base64 24)Aa1!"
docker run --detach --name mssql-js-benchmark \
  --cpuset-cpus 0-3 --memory 4g \
  --publish 127.0.0.1:15433:1433 \
  --env ACCEPT_EULA=Y --env MSSQL_PID=Developer \
  --env MSSQL_SA_PASSWORD="$SQL_PASSWORD" \
  mcr.microsoft.com/mssql/server:2025-latest
```

Wait for SQL Server's "ready for client connections" message in
`docker logs mssql-js-benchmark`, then run:

```bash
taskset -c 8-11 node compare-tedious.js
docker rm --force mssql-js-benchmark
unset SQL_PASSWORD
```

Choose disjoint CPU sets appropriate for your machine; `taskset` is Linux-only
and optional. On WSL without Docker Desktop integration, `docker.exe` can be
used instead if its published loopback port is reachable.

### Method

The workloads cover connect/query/close, reused-connection `SELECT 1`,
an integer-parameter lookup, 1,000 and 10,000 two-integer rows, 1,000 string
rows (an ID, short label, and 512-character Unicode body), a 1 MiB binary
value, and `SELECT 1` across eight pre-opened connections. Parameterized
10,000-row, 100,000-row, 1,000-string-row, and eight-connection lookup variants exercise
the parameterized result path separately.

- Both drivers use SQL authentication, TLS with certificate verification
  bypassed for the disposable server, and request 8,000-byte TDS packets.
  Negotiated packet size and protocol version must match within each pair.
  Queries use SQL batches; the parameterized workload uses `sp_executesql`
  for both.
- Tedious row events are collected into named JavaScript objects, matching
  `mssql-js`'s materialized result shape. Every cell is compared with expected
  data before each sample, and every timed operation checks the row count.
- Reused connections get identical session SET options outside timing.
  Connection-churn timing includes each driver's default login setup,
  `SELECT 1`, and close; module loading is excluded.
- Each driver/workload/round runs in a fresh child process. Driver order
  alternates between paired rounds, with a one-second warmup before each
  three-second measurement. GC is not forced. There are six rounds by default.
- Throughput and latency include query execution, transfer, decoding, result
  materialization, and row-count consumption. Concurrent latency is per query,
  not per eight-query batch.

The console reports median throughput and the median of paired
`mssql-js / tedious` throughput ratios; values above 1 favor `mssql-js`.
The range is the observed range of paired ratios, **not a confidence interval**.
`results/comparison.json` retains every sample, server/session information,
client CPU time, and process peak RSS. Peak RSS includes startup and warmup;
it is not an allocation-per-query metric. Results are ignored by Git.

| Environment variable                  | Default                                              |
| ------------------------------------- | ---------------------------------------------------- |
| `SQL_PASSWORD`                        | Required                                             |
| `DB_HOST` / `DB_PORT`                 | `127.0.0.1` / `15433`                                |
| `BENCH_ROUNDS`                        | `6`                                                  |
| `BENCH_WARMUP_MS` / `BENCH_SAMPLE_MS` | `1000` / `3000`                                      |
| `BENCH_SCENARIOS`                     | All; optional comma-separated workload names         |
| `BENCH_OUTPUT`                        | `perf/results/comparison.json`                       |
| `BENCH_BASELINE_PATH`                 | Unset; optional preserved baseline `dist` directory  |
| `BENCH_CANDIDATE_PATH`                | `mssql-js/dist`; optional candidate `dist` directory |

For a quick correctness run, set `BENCH_ROUNDS=1 BENCH_WARMUP_MS=100
BENCH_SAMPLE_MS=100`. Do not use those short samples as performance results.
Record the SQL image digest and container resources alongside published
results. A local container comparison is not a production-network or
dedicated-machine benchmark; this suite does not cover writes, bulk copy,
streaming/backpressure, pooling, or all SQL data types.

### Comparing an optimization against the previous build

Before making changes, preserve the release addon and compiled API together.
From `mssql-js`:

```bash
mkdir -p perf/results/baseline
cp -a dist perf/results/baseline/
cp package.json perf/results/baseline/
```

The copied `package.json` keeps the snapshot's compiled CommonJS files from
inheriting the perf directory's ES-module configuration. After building the
candidate, run the same workloads against all three drivers:

```bash
BENCH_BASELINE_PATH="$PWD/perf/results/baseline/dist" \
BENCH_OUTPUT="$PWD/perf/results/optimization.json" \
taskset -c 8-11 node perf/compare-tedious.js
```

With a baseline, six rounds cover all six driver orders. Each child loads
only its selected driver version. Results include native-binary and wrapper
hashes, plus paired candidate/baseline ratios under `versusBaseline`.
Use a fresh output path for each experiment and do not compare candidate
numbers with a baseline measured on a different run.

### Before optimization: September 10, 2026

Release build of commit `182af9260b66b8f4d7d752683e55588f40b0f368`,
Node.js 22.23.2, Tedious 20.0.0, and SQL Server 2025 CU8-GDR
(`17.0.4085.5`). The client ran under WSL on an Intel Xeon Platinum 8370C;
SQL Server ran in Docker Desktop with 4 GiB RAM and logical CPUs 0-3.
The client used logical CPUs 8-11. Both sessions negotiated TLS and
8,170-byte packets. These are local-container measurements, not general
performance guarantees.

| Workload                    | mssql-js ops/s | Tedious ops/s | Median paired ratio |
| --------------------------- | -------------: | ------------: | ------------------: |
| Connect, SELECT 1, close    |            8.5 |           8.6 |               0.98x |
| SELECT 1, reused connection |          895.1 |         854.3 |               1.05x |
| Parameterized lookup        |          588.2 |         848.4 |               0.69x |
| 1,000 integer rows          |          616.0 |         388.9 |               1.56x |
| 10,000 integer rows         |          157.4 |          90.0 |               1.78x |
| 1,000 string rows           |          136.2 |         120.5 |               1.13x |
| 1 MiB binary value          |          162.6 |         158.5 |               1.03x |
| SELECT 1, eight connections |        5,748.5 |       3,969.5 |               1.44x |

The integer-result, string-result, and concurrent-query advantages appeared
in all six pairs. The parameterized lookup was consistently slower
(0.64-0.76x throughput). Connect/query/close, single-connection SELECT 1,
and the binary value had paired ranges crossing 1.0; treat these as near
ties rather than reliable wins.

### Parameterized-query optimization result

The retained changes are confined to `mssql-js`: execute and collect buffered
parameterized queries in one native call, cache the native connection's
immutable encoding snapshot, and project decoded values directly into the
final row objects using one reusable scratch row. The public `Request` result
shape and the low-level raw/chunked APIs are preserved.

The final comparison used the same host/container configuration above and an
unchanged release build of `182af926` as the baseline. Six rounds covered all
three-driver orders, with a one-second warmup and two-second measurement per
leg. Throughput below is median queries/second; improvement is the median
paired candidate/baseline ratio.

| Parameterized workload    |  Before |   After | Tedious | Improvement |
| ------------------------- | ------: | ------: | ------: | ----------: |
| Single-row lookup         |   584.2 |   879.9 |   850.0 |       1.50x |
| Lookup, eight connections | 4,496.5 | 5,631.2 | 3,316.7 |       1.25x |
| 10,000 integer rows       |   145.3 |   163.3 |    87.1 |       1.11x |
| 100,000 integer rows      |    19.0 |    21.2 |     8.2 |       1.12x |

All six pairs showed improvements for these four workloads. Single-row lookup
median latency fell from 1.67 ms to 1.09 ms, and client CPU per query fell from
1.23 ms to 0.51 ms. Non-parameterized controls and parameterized string results
had changes within the observed run variation.

Independent experiments isolated the fused-call gain and a further concurrent
benefit from encoding caching. Fusion alone introduced a small 100,000-row
slowdown; direct row projection removed it and improved the final result.
A 512-byte initial native buffer and preallocated JS row arrays did not show
reliable throughput gains and were not retained.

The full nine-workload results are in
`results/optimization/final-comparison.json`, with per-experiment results in
the same ignored directory. These local measurements are not production
performance guarantees.

## Original single-driver benchmark

This perf / benchmark can be run with `node query.mjs`

The output would look something like the following:

```
SQL Server Query x 9.55 ops/sec ±1.53% (49 runs sampled)
Fastest is SQL Server Query
```

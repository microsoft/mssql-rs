# ConnectorX TPC-H A/B benchmark

Measures the effect of [ConnectorX](https://github.com/sfu-db/connector-x) moving
its SQL Server source from Tiberius to `mssql-tds`
([sfu-db/connector-x#942](https://github.com/sfu-db/connector-x/issues/942)),
using the workload from ConnectorX's own
[benchmark](https://github.com/sfu-db/connector-x/blob/main/Benchmark.md#tpc-h):
`SELECT * FROM lineitem` at TPC-H scale factor 10, partitioned on `l_orderkey`.

Runs on the dedicated perf lab through
[`connectorx-tpch-perf-linux-pipeline.yml`](../../.pipeline/connectorx-tpch-perf-linux-pipeline.yml).

## Arms

| Arm | Package | Driver |
|---|---|---|
| `before` | `connectorx==0.4.6` | Tiberius (only driver in that release) |
| `now-tiberius` | `connectorx==0.4.7a1` | Tiberius, via `cx.mssql_driver = "tiberius"` |
| `now` | `connectorx==0.4.7a1` | `mssql-tds` (default) |

The summary reports three comparisons:

- **before → now**: what a user sees on upgrade.
- **now-tiberius → now**: the driver alone, same wheel.
- **before → now-tiberius**: control; should be ~1.0×, otherwise non-driver
  release changes are contributing to the headline number.

Each scenario (`encrypt` × `partition_num` × `return_type`) runs one discarded
warm-up round, then N measured rounds with the arm order shuffled each round.
Every read is a fresh Python process pinned to the client cores
(`PERF_CLIENT_CPUS`), so peak RSS belongs to that read. The first read of each
arm queries `sys.dm_exec_sessions` / `sys.dm_exec_connections` to record the
client interface and encryption actually seen by SQL Server; the run fails if
the driver switch does not change the client interface name.

`encrypt=false` is login-only TLS for both drivers (`0x00` prelogin), and
`encrypt=true&trust_server_certificate=true` is full-session TLS for both.

## Setup on the VM

`run-benchmarks.sh` generates lineitem with `tpchgen-cli`, bulk-loads it with
`bcp` into a `tpch` database on the local temp SSD (clustered on
`l_orderkey, l_linenumber`), verifies the row count, then runs `bench.py`.

## Parameters

Pipeline parameters map to `KEY=VALUE` script arguments:

| Script arg | Default | Meaning |
|---|---|---|
| `SF` | `10` | TPC-H scale factor |
| `ROUNDS` | `5` | Measured rounds per arm and scenario |
| `WARMUP_ROUNDS` | `1` | Discarded rounds per scenario |
| `PARTITIONS` | `1,4` | `partition_num` values (`1` = no partitioning) |
| `ENCRYPT_MODES` | `false,true` | `encrypt=` values |
| `RETURN_TYPES` | `arrow` | `arrow` and/or `pandas` |
| `CX_BEFORE` | `0.4.6` | `connectorx` version for `before` |
| `CX_AFTER` | `0.4.7a1` | `connectorx` version for `now` / `now-tiberius` |

`arrow` is the default because the pandas conversion is identical across arms
and only dilutes the driver delta.

## Output (`results/`)

- `summary.md`: comparison tables, rendered on the run's Summary tab.
- `comparison.json`: the same data, machine-readable.
- `raw.jsonl`: every sample, including warm-ups.
- `environment.txt`: CPU, memory, SQL Server version, `pip freeze` for both venvs.

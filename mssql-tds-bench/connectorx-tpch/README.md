# ConnectorX TPC-H A/B benchmark

Measures the effect of [ConnectorX](https://github.com/sfu-db/connector-x) moving
its SQL Server source from Tiberius to `mssql-tds`
([sfu-db/connector-x#942](https://github.com/sfu-db/connector-x/issues/942)),
using the workload from ConnectorX's own
[benchmark](https://github.com/sfu-db/connector-x/blob/main/Benchmark.md#tpc-h):
`SELECT * FROM lineitem` at TPC-H scale factor 10, partitioned on `l_orderkey`.

On this branch the Linux perf-lab pipeline
([`perf-baseline-linux-pipeline.yml`](../../.pipeline/perf-baseline-linux-pipeline.yml))
runs this harness instead of the Criterion benches.

## Arms

| Arm | Package | Driver |
|---|---|---|
| `baseline` | `connectorx==0.4.6` | Tiberius (only driver in that release) |
| `candidate` | `connectorx==0.4.7a1` | `mssql-tds` (default) |
| `candidate-tiberius` (`INCLUDE_CONTROL=1`) | `connectorx==0.4.7a1` | Tiberius, via `cx.mssql_driver = "tiberius"` |

The control arm separates the driver from other changes between the two
releases: `baseline → candidate-tiberius` should be ~1.0×.

Each scenario (`encrypt` × `partition_num` × `return_type`) runs one discarded
warm-up round, then N measured rounds with the arm order shuffled each round.
Every read is a fresh Python process pinned to the client cores
(`PERF_CLIENT_CPUS`), so peak RSS belongs to that read. The first read of each
arm queries `sys.dm_exec_sessions` / `sys.dm_exec_connections` to record the
client interface and encryption seen by SQL Server; the run fails if the
Tiberius and `mssql-tds` arms report the same client interface name.

`encrypt=false` is login-only TLS for both drivers (`0x00` prelogin), and
`encrypt=true&trust_server_certificate=true` is full-session TLS for both.

## Setup on the VM

`run-benchmarks.sh` generates lineitem with `tpchgen-cli`, bulk-loads it with
`bcp` into a `tpch` database on the local temp SSD (clustered on
`l_orderkey, l_linenumber`), verifies the row count, then runs `bench.py`.

## Parameters

Set through the pipeline's `testScriptArgs` as space-separated `KEY=VALUE`:

| Arg | Default | Meaning |
|---|---|---|
| `SF` | `10` | TPC-H scale factor |
| `ROUNDS` | `5` | Measured rounds per arm and scenario |
| `WARMUP_ROUNDS` | `1` | Discarded rounds per scenario |
| `PARTITIONS` | `1,4` | `partition_num` values (`1` = no partitioning) |
| `ENCRYPT_MODES` | `false,true` | `encrypt=` values |
| `RETURN_TYPES` | `arrow` | `arrow` and/or `pandas` |
| `CX_BASELINE` | `0.4.6` | `connectorx` version for `baseline` |
| `CX_CANDIDATE` | `0.4.7a1` | `connectorx` version for `candidate` |
| `INCLUDE_CONTROL` | `0` | `1` adds the `candidate-tiberius` arm |

`arrow` is the default because the pandas conversion is identical across arms
and only dilutes the driver delta.

## Output (`results/`)

- `summary.md`: comparison tables, rendered on the run's Summary tab.
- `comparison.json`: the same data, machine-readable.
- `raw.jsonl`: every sample, including warm-ups.
- `environment.txt`: CPU, memory, SQL Server version, `pip freeze` for both venvs.
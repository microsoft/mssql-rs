# mssql-tds-bench

A **fixed Criterion harness** for benchmarking [`mssql-tds`](../mssql-tds) with a
**swappable dependency**, so a stable baseline build and a candidate build can be
compared on the same host with `mssql-tds` as the only variable.

See [`../mssql-tds/docs/perf-testing-plan.md`](../mssql-tds/docs/perf-testing-plan.md)
for the full design and rationale.

## How isolation works

The harness code, Criterion version, and toolchain stay byte-for-byte identical
across runs. The `mssql-tds` dependency in [`Cargo.toml`](Cargo.toml) always points
at `../mssql-tds`; isolation comes from swapping the **source** at that path between
runs rather than building a different harness:

- **candidate** — `../mssql-tds` is the current working tree.
- **baseline** — `../mssql-tds` is replaced in place with a checkout of the
  `perf-baseline` git tag (see [`perf-lab/run-benchmarks.sh`](perf-lab/run-benchmarks.sh)).

Any statistically significant delta is therefore attributable to `mssql-tds`
itself. The baseline is pinned by the **`perf-baseline` git tag**, which is moved
manually in the repository when the baseline should advance.

## Benchmarks

| Bench file | Scenarios |
|------------|-----------|
| `connection` | single connect/close; N concurrent connects (50/100/500) |
| `query` | simple SELECT; N-row reads (100/1k/10k); single INSERT; `sp_executesql`; stored procedure; batched statements |
| `datatypes` | primitives; VARCHAR/NVARCHAR; temporal types; LOB (1 MB/20 MB, byte throughput) |
| `bulk` | `BulkCopy` insert across batch sizes 50/500/5,000 |
| `tds_specific` | packet-size sensitivity (4096/8192/32768); zero-copy `next_row` row-iteration throughput |

Each benchmark creates its own session temp tables / temp procedures, so **no
server-side setup script is required** — the objects vanish when the connection
closes.

## Running locally

```sh
# Run everything (real SQL Server required)
cargo bench -p mssql-tds-bench

# A single bench file
cargo bench -p mssql-tds-bench --bench query

# Save a named baseline, then compare
cargo bench -p mssql-tds-bench -- --save-baseline base
# (swap the ../mssql-tds source to the candidate, re-run — see perf-lab/run-benchmarks.sh)
cargo bench -p mssql-tds-bench -- --save-baseline candidate
critcmp base candidate
```

When no server is reachable, each benchmark probes the connection once and skips
gracefully (this is what lets the bench binaries run as tests in CI without a DB).

## Environment contract

Shared with `mssql-tds/benches/perf.rs`:

| Variable | Purpose |
|----------|---------|
| `DB_HOST`, `DB_PORT` | connection target |
| `DB_USERNAME`, `SQL_PASSWORD` | credentials (`SQL_PASSWORD` falls back to `/tmp/password`) |
| `TRUST_SERVER_CERTIFICATE`, `CERT_HOST_NAME` | TLS validation |
| `BENCH_ENCRYPT` (`strict`\|`on`\|`off`, default `on`) | encryption setting |

Criterion tuning knobs (defaults chosen for noisy, network-bound runs):

| Variable | Default | Purpose |
|----------|---------|---------|
| `BENCH_SECS` | `15` | measurement time per benchmark (seconds) |
| `BENCH_SAMPLES` | `10` | sample size |
| `BENCH_SIGNIFICANCE` | `0.05` | significance level |
| `BENCH_NOISE` | `0.05` | noise threshold |
| `BENCH_BULK_ROWS` | `10000` | rows for the bulk-insert bench |
| `BENCH_ITER_ROWS` | `50000` | rows for the row-iteration bench |

## Fixed-baseline comparison on the perf lab

`perf-lab/run-benchmarks.sh` (Linux) and `perf-lab/run-benchmarks.ps1` (Windows)
are the testScripts run by the shared `PerfTest` lab template
(`.pipeline/perf-baseline-pipeline.yml`). They run on a dedicated perf-lab VM and:

1. Build and run the candidate (`mssql-tds` = working tree) with `--save-baseline candidate`.
2. Replace the `mssql-tds` source at `../mssql-tds` in place with a local
   `git worktree` checkout of the `perf-baseline` tag, and run `--save-baseline base`.
3. `critcmp base candidate` into `results/comparison.txt`.

The harness is always built from the candidate tree (it does not exist at the
baseline tag); only the `mssql-tds` *source* is swapped, so the harness, Criterion
version, and toolchain stay constant. `SQL_SERVER` and `SQL_PASSWORD` are injected
by the lab template. The `perf-baseline` tag is moved manually in the git repo to
advance the baseline.

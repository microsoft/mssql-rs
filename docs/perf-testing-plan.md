# Plan: Performance Testing for mssql-tds

## TL;DR

Adopt **Criterion.rs** as the benchmarking framework for `mssql-tds` (the closest
Rust analog to BenchmarkDotNet, already used in the repo). Add a **new, additive
harness crate** (`mssql-tds-bench`) that pins `mssql-tds` as a *swappable* dependency
so a stable baseline version and a candidate build can be benchmarked on the same
host with **mssql-tds as the only variable**. The comparison runs on a **dedicated
perf-lab host** via the shared `PerfTest` lab `extends` template. This complements the
existing PR-vs-target benchmark pipeline. Cross-driver spec reporting and mock-TDS
microbenchmarks are out of scope for this round.

---

## 1. Framework Recommendation

**Use Criterion.rs as the primary framework.** It is the closest Rust analog to
BenchmarkDotNet: statistics-driven, throughput measurement, parameterized inputs,
async support via the `async_tokio` feature, and regression detection against saved
baselines. It is already wired into the repo at `mssql-tds/benches/perf.rs`.

One genuine gap: Criterion reports mean/median + confidence intervals, **not**
arbitrary P50/P95/P99 percentiles. That only matters for cross-driver spec-compliant
JSON, which is out of scope here.

### Framework landscape

| Framework | Stars | Strengths | Fit |
|-----------|-------|-----------|-----|
| **Criterion.rs** | 5.5k | Statistics-driven, throughput, parameterized, async (tokio), baseline comparison, HTML reports | **Primary** — BenchmarkDotNet analog, already in repo |
| Divan | 1.4k | Simpler, faster, allocation profiling | Deferred — async support more manual, less mature |
| iai-callgrind | — | Instruction-count (Cachegrind), deterministic, CI noise-immune | Optional later — Linux/valgrind only, not wall-clock |

---

## 2. Regression Architecture

### The isolation requirement

The goal is to compare two builds of `mssql-tds` in the same environment, running the
exact same tests, with **mssql-tds itself as the only variable**, so a measured delta
is attributable to library code changes.

The existing benchmarks live *inside* the `mssql-tds` crate. Comparing two versions by
`git checkout`-ing each one also changes the benchmark harness code, the Criterion
version, and the toolchain along with the library — so a difference can't be cleanly
attributed to library code. The existing PR pipeline does a whole-repo checkout of the
target branch, which has this limitation.

### The fix: a fixed harness crate with a swappable dependency

Add `mssql-tds-bench`, a new workspace member whose benchmark code stays fixed. Only
its `mssql-tds` dependency is swapped between runs:

```toml
# mssql-tds-bench/Cargo.toml — candidate run
mssql-tds = { path = "../mssql-tds" }
# baseline run (swap to a local worktree of the perf-baseline tag):
mssql-tds = { path = "/tmp/perf-baseline/mssql-tds" }
```

Because the harness crate, Criterion version, and benchmark code are byte-for-byte
identical across both runs, any statistically significant delta is attributable to
`mssql-tds`. The harness is **always built from the candidate tree** (it is brand new
and does not exist at the baseline tag); only its `mssql-tds` *source* changes. On the
perf VM the baseline source is materialized with a local `git worktree` of the
`perf-baseline` tag from the shipped `.git`, so no VM-side ADO authentication is needed.

### Criterion baseline commands

```sh
cargo bench -p mssql-tds-bench -- --save-baseline base       # baseline (version A)
# swap mssql-tds dependency to candidate
cargo bench -p mssql-tds-bench -- --save-baseline candidate  # candidate (version B)
critcmp base candidate                                       # side-by-side comparison
```

### Practical constraints

- **Same machine, same run.** Criterion comparison is only meaningful when both runs
  happen back-to-back on the same isolated host — here a **dedicated perf-lab VM** provisioned
  by the shared `PerfTest` lab template. Archived baseline *numbers* from a
  different VM/run are rejected: these benchmarks are network-bound, so cross-run drift
  (machine, network, SQL Server state) dwarfs the code delta. Version A must be rebuilt
  live on the same host each run.
- **Noise tuning.** Network-bound, end-to-end benchmarks are noisier than pure-CPU
  microbenchmarks. Set deliberate `significance_level` / `noise_threshold` and per-group
  `sample_size` / `measurement_time` to avoid false regression alarms.
- **Rust reality.** `mssql-tds` is a static `rlib`, not a redistributable shared binary.
  There is no prebuilt binary to consume; the "published artifact" analog is a published
  crate version (still compiled from source) — deferred in favor of a git tag.

---

## 3. Benchmark Scenarios

End-to-end against a real SQL Server. Where only execution should be measured,
database setup is moved to Criterion's setup phase.

### Connection
- Single connect/close (non-pooled). *(Pooled connection excluded — mssql-tds has no pool.)*
- N concurrent connects (N = 50, 100, 500).

### Query execution
- Simple SELECT (single row).
- SELECT N rows (100 / 1,000 / 10,000) with mixed primitive types.
- Single INSERT.
- Parameterized query via `execute_sp_executesql`.
- Stored procedure via `execute_stored_procedure`.
- Batched statements in a single round-trip.

### Data types
- Primitives (BIT / TINYINT / SMALLINT / INT / BIGINT, DECIMAL / FLOAT).
- VARCHAR / NVARCHAR.
- DATETIME2 / DATE / TIME / DATETIMEOFFSET.
- LOB up to 20 MB (VARCHAR(MAX) / NVARCHAR(MAX) / VARBINARY(MAX)), `Throughput::Bytes`.

### Bulk
- Bulk insert 10,000–100,000 rows via the `BulkCopy` builder, over batch sizes 50 / 500 / 5,000.

### mssql-tds-specific
- Packet-size sensitivity (4096 / 8192 / 32768) on large reads — measures reassembly overhead.
- Zero-copy `next_row` row-iteration throughput at large row counts.

---

## 4. Implementation Phases

1. **Additive harness crate.** Create `mssql-tds-bench` as a new workspace member.
   Leave the existing `mssql-tds/benches/perf.rs` and its `[[bench]]` entry untouched
   (the existing pipeline references `--bench perf`). Swappable `mssql-tds` dependency
   (`path` = candidate, git `tag` = baseline). Enable Criterion `async_tokio`; extract a
   shared `bench_support` module (connection context + env config) from the current
   `create_context()`.
2. **Scenario coverage.** Implement the scenarios in Section 3, with connection setup
   moved out of the measured path where applicable.
3. **Test data & config.** `setup.sql` for the benchmark table/proc and seed rows. Reuse
   the existing env contract (`DB_HOST`, `DB_PORT`, `DB_USERNAME`, `SQL_PASSWORD`,
   `TRUST_SERVER_CERTIFICATE`, `CERT_HOST_NAME`).
4. **Baseline-pin mechanism.** Pin version A as the `perf-baseline` git tag. On the perf
   VM the baseline `mssql-tds` source is materialized via a local `git worktree` of that
   tag (from the shipped `.git`), and the harness's `mssql-tds` path dependency is swapped
   to it. The tag name is **hard-coded into the testScript**; the `perf-baseline` tag is
   **moved manually in the git repo** when the baseline should advance (per release or when
   an intentional perf change lands). Tune noise thresholds. Compare via `critcmp`.
5. **Perf-lab pipeline (complementary).** Consume the shared `PerfTest` lab template
   (`v1/Perf.Test.Job.yml@PerfTemplates`) as a pure `extends`, so benchmarks run on a
   **dedicated host VM** instead of a shared build agent. The pipeline only passes
   parameters (`platform`, `testRootDir` = repo root, `testScript`, results subdir,
   timeout); the template deploys the VM, copies the repo, runs the testScript, collects
   `results/`, and tears the VM down. The build happens **on the VM** (the lab's documented
   model): `mssql-tds-bench/perf-lab/run-benchmarks.{sh,ps1}` bootstraps the toolchain,
   runs the candidate (`--save-baseline candidate`), creates the baseline worktree and
   runs it (`--save-baseline base`), then `critcmp base candidate`, writing
   `results/comparison.txt`. SQL connection (`SQL_SERVER`/`SQL_PASSWORD`) is injected by
   the template. Supports Linux (default) or Windows via the `platform` parameter.

### Verification
1. `cargo bench -p mssql-tds-bench` runs all scenarios against a real SQL Server.
2. Swap the dependency to the baseline tag, run, and `critcmp base candidate` confirms
   identical scenario IDs compare.
3. Deliberately slowing a candidate build flags a regression only on affected scenarios.
4. New pipeline dry-run produces the comparison artifact on a dedicated perf-lab VM; the
   existing PR-gate pipeline still passes unchanged.

---

## 5. Scope & Decisions

- Existing `mssql-tds/benches/perf.rs` stays in place (delete later); the new harness is additive.
- The fixed-baseline mechanism **complements** the existing PR-vs-target pipeline
  (`.pipeline/benchmark-pipeline.yml`).
- Baseline pinned via a **git tag materialized as a local worktree on the perf VM** (not a
  whole-repo checkout or a Cargo git dependency) so mssql-tds is the only variable and no
  VM-side ADO auth is required.
- Baseline tag name is **`perf-baseline`, hard-coded in the testScript**. The tag is
  **moved manually in the git repo** to advance the baseline (no committed manifest or
  pipeline variable).
- Perf-lab pipeline runs on a **dedicated host VM** via the shared `PerfTest` lab
  `extends` template; the build happens on the VM (the lab's documented model).
- End-to-end (real SQL Server) only.
- **Out of scope:** cross-driver spec-compliant JSON (P50/P95/P99), mock-TDS
  microbenchmarks, pooled-connection scenario, Always Encrypted / Entra ID auth.

---

## 6. Future Considerations

### Dependency / toolchain regressions (longitudinal trend)

The git-tag isolation deliberately holds `Cargo.lock` and `rust-toolchain.toml` constant,
so it will **not** catch a regression introduced by a dependency bump (e.g., `tokio`,
`native-tls`) or a compiler upgrade. These are two different questions:

| Question | What varies | What's held constant | Mode |
|----------|-------------|----------------------|------|
| "Did *my code* regress?" | mssql-tds source only | deps, toolchain, harness | Isolated (git-tag baseline) |
| "Did the *shipped artifact* get slower for any reason?" | code + deps + toolchain | nothing | Longitudinal trend of main |

A future **longitudinal trend mode** would track the tip of `main` over time (nothing
held constant), store results across builds, and watch for drift — catching slow creep
from dependency/toolchain changes that the isolated mode hides. Three modes in total:
(1) existing PR-vs-target, (2) new isolated git-tag baseline, (3) trend of main.

### Future: perf testing mssql-odbc (Rust ODBC driver)

Criterion benchmarks Rust functions compiled into the same test binary (in-process,
Rust-native). The future `mssql-odbc` will be a `cdylib` exposing the C ABI
(`SQLConnect`, `SQLExecDirect`, `SQLFetch`, …), normally consumed through an ODBC Driver
Manager (unixODBC / Windows DM). The representative path is *through the driver manager*,
the way a real ODBC app uses it.

A Rust bench *could* load the driver via FFI (e.g., `odbc-api`) with Criterion as the
timing engine, but that loses cross-driver comparability and doesn't measure the real
consumer path. The cross-driver spec already prescribes a **C/C++ ODBC harness**
(`QueryPerformanceCounter` / `clock_gettime`) for ODBC. Sharing that harness with
`msodbcsql` enables an apples-to-apples **Rust mssql-odbc vs native msodbcsql** comparison.

This points to a layered strategy:

- **Layer 1 — mssql-tds (Rust lib):** Criterion component/micro benchmarks → attributes
  protocol/codec/parsing regressions to source. *(This plan.)*
- **Layer 2 — mssql-odbc (cdylib via ODBC DM):** end-to-end benchmarks via the C/C++ ODBC
  harness shared with msodbcsql → Rust-vs-native ODBC comparison. *(Future.)*
- **Cross-cutting — longitudinal trend of main:** catches dependency/toolchain drift that
  Layer 1's isolation hides.

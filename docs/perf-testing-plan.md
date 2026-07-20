# Plan: Performance Testing for mssql-tds

## TL;DR

Adopt **Criterion.rs** as the benchmarking framework for `mssql-tds` (the closest
Rust analog to BenchmarkDotNet, already used in the repo). Add a **new, additive
harness crate** (`mssql-tds-bench`) that pins `mssql-tds` as a *swappable* dependency
so a stable baseline version and a candidate build can be benchmarked on the same
host with **mssql-tds as the only variable**. The comparison runs on a **dedicated
perf-lab host** via the shared `PerfTest` lab `extends` template, on **both Linux and
Windows** (separate pipeline definitions). Because the workload is network-bound, the
harness **interleaves the candidate and baseline runs per bench binary** and layers
several noise controls (client CPU pinning, warm-up, auto-confirm of tripped
benchmarks) so the comparison is unbiased and the regression gate is trustworthy —
see §2. This complements the existing PR-vs-target benchmark pipeline. Cross-driver
spec reporting and mock-TDS microbenchmarks are out of scope for this round.

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

### The fix: a fixed harness crate with a swapped dependency *source*

Add `mssql-tds-bench`, a new workspace member whose benchmark code stays fixed. Its
`mssql-tds` dependency always points at `../mssql-tds`; the *source* at that path is
what changes between runs:

```toml
# mssql-tds-bench/Cargo.toml — constant across both runs
mssql-tds = { path = "../mssql-tds" }
```

- **candidate run** — `../mssql-tds` is the current working tree.
- **baseline run** — `../mssql-tds` is replaced in place with a checkout of the
  commit pinned in `mssql-tds-bench/perf-lab/baseline-commit.txt`.

Because the harness crate, Criterion version, and benchmark code are byte-for-byte
identical across both runs, any statistically significant delta is attributable to
`mssql-tds`. The harness is **always built from the candidate tree** (it is brand new
and does not exist at the baseline commit); only the `mssql-tds` *source* changes. On the
perf VM the baseline source is materialized with a local `git worktree` of the
baseline commit from the shipped `.git`, then copied over `../mssql-tds` for the
baseline run — so no VM-side ADO authentication is needed. (Swapping the source in
place, rather than re-pointing the dependency at the worktree, keeps a single
`mssql-tds` in the workspace and avoids a Cargo lockfile package collision.)

### Comparison flow: interleaved per-binary runs

Conceptually the comparison is Criterion's saved-baseline workflow:

```sh
# (conceptual) baseline = version A source at ../mssql-tds, candidate = version B
cargo bench -p mssql-tds-bench -- --save-baseline base
cargo bench -p mssql-tds-bench -- --save-baseline candidate
critcmp base candidate
```

In practice the harness does **not** run all-baseline-then-all-candidate. Running one
full side and then the other leaves the two measurements of any given benchmark tens of
minutes apart, and slow host drift (CPU frequency, SQL Server cache / tempdb state,
thermals) over that gap shows up as a systematic skew — the *second* side run looks
uniformly slower even on byte-identical code. Because that bias is one-directional it
silently desensitizes the gate.

The final harness instead **builds both sides up front and interleaves per bench
binary**: for each bench binary it runs the candidate build and the baseline build
back-to-back, both writing into the shared `target/criterion`, before moving to the next
binary. Keeping each benchmark's two measurements adjacent in time cancels the drift, so
the comparison is symmetric (the baseline column sits at ~1.00 across the board) and the
gate is unbiased.

Interleaving is per **binary**, not per individual benchmark: a per-benchmark filter
still re-runs every bench function's Criterion setup (temp tables / procs, seed rows)
each time, so per-binary keeps total setup cost — and run time — the same as the old
two-pass approach. For the same reason the benchmarks are split across several small
bench binaries (e.g. `query_rows` / `query_procs` / `query_prepared`) rather than one
large one: a single oversized binary has a wide internal candidate→baseline time span
that reintroduces the very drift interleaving removes.

Both sides are built into separate target dirs (`target/` candidate, `target-base/`
baseline) so both persist; the baseline `mssql-tds` source is materialized via a local
`git worktree` of the pinned commit and swapped over `../mssql-tds` only for the baseline
build.

### Practical constraints

- **Same machine, same run.** Criterion comparison is only meaningful when both runs
  happen back-to-back on the same isolated host — here a **dedicated perf-lab VM** provisioned
  by the shared `PerfTest` lab template. Archived baseline *numbers* from a
  different VM/run are rejected: these benchmarks are network-bound, so cross-run drift
  (machine, network, SQL Server state) dwarfs the code delta. Version A must be rebuilt
  live on the same host each run.
- **Noise tuning (what actually moved the needle).** Network-bound, end-to-end
  benchmarks are far noisier than pure-CPU microbenchmarks, so the harness layers several
  controls, each added in response to an observed artifact:
  - **Interleaved per-binary runs** (above) — removes the one-directional
    baseline-slower skew from host drift; the single biggest correctness fix.
  - **Client CPU pinning** — the benchmark client is pinned (Linux `taskset -c`, Windows
    `ProcessorAffinity`) to a core set **disjoint** from the cores SQL Server is pinned
    to, published by the lab as `PERF_CLIENT_CPUS`, so the two do not fight for CPUs.
    Pinning is set once on the parent process; children inherit.
  - **Discarded warm-up pass** before the interleaved run to prime SQL Server's buffer
    pool / plan cache and the OS page cache, so the first measured benchmark isn't paying
    cold-start cost.
  - **Auto-confirm (best-of-N)** — a strict gate can trip on a transient single-benchmark
    outlier (short, CPU-bound benches can swing double digits on a shared VM), so any
    benchmark that trips the threshold is **re-measured `BENCH_CONFIRM_RUNS` times**
    (default 4, interleaved, offenders only) and kept as a regression only if it trips in a
    **majority** of those re-runs (`BENCH_CONFIRM_QUORUM`, default `N/2+1`). A real
    regression reproduces consistently; a one-off spike does not and is reported as
    transient noise, not a failure.
  - **Release-grade sampling** — heavier warm-up / measurement time / sample count than
    the fast local defaults (`BENCH_WARMUP_SECS` / `BENCH_SECS` / `BENCH_SAMPLES`),
    letting caches settle and separating small real deltas from noise.
  - **Allocator tuning** for the multi-MB LOB benches (raise glibc
    `MALLOC_MMAP_THRESHOLD_`, disable trimming) so each iteration reuses heap memory
    instead of re-`mmap`-ing and re-faulting 20 MB every time.
  - **Kernel network tuning** (widen the ephemeral port range, enable `tcp_tw_reuse`) so
    the connection-churn benchmark doesn't exhaust local ports.
  - **Diagnostics** — a SQL Server configuration snapshot (memory / MAXDOP / cost
    threshold / affinity / tempdb / trace flags) and bracketing CPU frequency/temperature
    telemetry are captured each run to validate the instance is tuned and both passes ran
    under equivalent conditions.
- **Rust reality.** `mssql-tds` is a static `rlib`, not a redistributable shared binary.
  There is no prebuilt binary to consume; the "published artifact" analog is a published
  crate version (still compiled from source) — deferred in favor of a pinned git commit.

### Regression gate

The run **fails only when a *candidate* benchmark is slower than its baseline by at least
the threshold** (`BENCH_REGRESSION_RATIO`, default `1.10` = 10%). A baseline-slower
result never fails the gate — it can only mean the comparison lost sensitivity, which is
exactly what the interleaving + pinning + warm-up work keeps in check. Any benchmark that
trips is re-measured by **auto-confirm** `BENCH_CONFIRM_RUNS` times (default 4) and only
fails the run if it regresses in a **majority** of those re-runs (default 3 of 4). The
verdict and the full `critcmp` table are written to `results/summary.md`, which the lab
attaches to the run's **Summary** tab (`task.uploadsummary`) so the comparison renders
inline on the pipeline run page.

---

## 3. Benchmark Scenarios

End-to-end against a real SQL Server. Where only execution should be measured,
database setup is moved to Criterion's setup phase.

### Connection
- Single connect/close (non-pooled). *(Pooled connection excluded — mssql-tds has no pool.)*
- N concurrent connects (N = 50, 100).

### Query execution
Split across the `query_rows`, `query_procs`, and `query_prepared` bench binaries — small
binaries keep the per-binary interleaving effective (see §2).
- Simple SELECT (single row).
- SELECT N rows (100 / 1,000 / 10,000) with mixed primitive types.
- Single INSERT.
- Parameterized query via `execute_sp_executesql`.
- Stored procedure via `execute_stored_procedure`, plus a variant with an OUTPUT parameter.
- Prepared statements: prepare-once/execute-many (`sp_prepare` / `sp_execute`) and one-shot `sp_prepexec`.
- Batched statements in a single round-trip.

### Data types
- Primitives (BIT / TINYINT / SMALLINT / INT / BIGINT, DECIMAL / FLOAT).
- VARCHAR / NVARCHAR.
- DATETIME2 / DATE / TIME / DATETIMEOFFSET.
- LOB up to 20 MB (VARCHAR(MAX) / NVARCHAR(MAX) / VARBINARY(MAX)), `Throughput::Bytes`.

### Bulk
- Bulk insert (default 10,000 rows, override via `BENCH_BULK_ROWS`) via the `BulkCopy` builder, over batch sizes 500 / 5,000.

### mssql-tds-specific
- Packet-size sensitivity (4096 / 8192 / 32768) on large reads — measures reassembly overhead.
- Zero-copy `next_row` row-iteration throughput at large row counts.

---

## 4. Implementation Phases

1. **Additive harness crate.** Create `mssql-tds-bench` as a new workspace member.
   Leave the existing `mssql-tds/benches/perf.rs` and its `[[bench]]` entry untouched
   (the existing pipeline references `--bench perf`). Swappable `mssql-tds` dependency
   (`path` = candidate; the baseline is materialized as a local `git worktree` of the
   pinned commit and swapped over `../mssql-tds` for the baseline build). Enable Criterion
   `async_tokio`; extract a shared `bench_support` module (connection context + env
   config) from the current `create_context()`.
2. **Scenario coverage.** Implement the scenarios in Section 3, with connection setup
   moved out of the measured path where applicable.
3. **Test data & config.** `setup.sql` for the benchmark table/proc and seed rows. Reuse
   the existing env contract (`DB_HOST`, `DB_PORT`, `DB_USERNAME`, `SQL_PASSWORD`,
   `TRUST_SERVER_CERTIFICATE`, `CERT_HOST_NAME`).
4. **Baseline-pin mechanism.** Pin version A as a commit SHA committed to the repo in
   `mssql-tds-bench/perf-lab/baseline-commit.txt`. On the perf VM the baseline `mssql-tds`
   source is materialized via a local `git worktree` of that commit (from the shipped
   `.git`) and copied over `../mssql-tds` in place for the baseline run, leaving the
   harness and its `path = "../mssql-tds"` dependency untouched. The testScript **reads
   the SHA from that file**; advancing the baseline is a **pull request that edits the
   file**, so every move is reviewed, recorded in git history, and attributable (rather
   than an untracked `git tag -f`). Tune noise thresholds. Compare via `critcmp`.
5. **Perf-lab pipeline (complementary).** Consume the shared `PerfTest` lab template
   (`v1/Perf.Test.Job.yml@PerfTemplates`) as a pure `extends`, so benchmarks run on a
   **dedicated host VM** instead of a shared build agent. The pipeline only passes
   parameters (`platform`, `testRootDir` = repo root, `testScript`, results subdir,
   timeout); the template deploys the VM, copies the repo (including `.git` for the
   baseline worktree), runs the testScript, collects `results/`, and tears the VM down.
   The build happens **on the VM** (the lab's documented model):
   `mssql-tds-bench/perf-lab/run-benchmarks.{sh,ps1}` bootstraps the toolchain, builds
   **both** sides up front, then **interleaves** the candidate and baseline per bench
   binary (see §2) into the shared `target/criterion`, runs `critcmp`, **auto-confirms**
   any gate-tripping benchmark, and writes `results/comparison.txt` plus a
   `results/summary.md` that the lab surfaces on the run's Summary tab. SQL connection
   (`SQL_SERVER` / `SQL_PASSWORD`) is injected by the template. Linux and Windows are
   **two separate pipeline definitions** (`perf-baseline-linux-pipeline.yml` /
   `perf-baseline-windows-pipeline.yml`), each hard-coding its `platform`; Windows and
   Linux numbers are not comparable and are scheduled and gated independently.

### Verification
1. `cargo bench -p mssql-tds-bench` runs all scenarios against a real SQL Server.
2. Swap the dependency to the baseline commit, run, and `critcmp base candidate` confirms
   identical scenario IDs compare.
3. Deliberately slowing a candidate build flags a regression only on affected scenarios.
4. New pipeline dry-run produces the comparison artifact on a dedicated perf-lab VM; the
   existing PR-gate pipeline still passes unchanged.

---

## 5. Scope & Decisions

- Existing `mssql-tds/benches/perf.rs` stays in place (delete later); the new harness is additive.
- The fixed-baseline mechanism **complements** the existing PR-vs-target pipeline
  (`.pipeline/benchmark-pipeline.yml`).
- Baseline pinned via a **commit SHA materialized as a local worktree on the perf VM** (not a
  whole-repo checkout or a Cargo git dependency) so mssql-tds is the only variable and no
  VM-side ADO auth is required.
- Baseline commit is stored in **`mssql-tds-bench/perf-lab/baseline-commit.txt`, read by the
  testScript**. The baseline is advanced via a **pull request that edits that file**, so the
  change is reviewed and recorded in git history (no untracked tag move or pipeline variable).
- Perf-lab pipeline runs on a **dedicated host VM** via the shared `PerfTest` lab
  `extends` template; the build happens on the VM (the lab's documented model).
- Comparison is **interleaved per bench binary** (not two full passes), and the run
  **fails only on a candidate-slower regression ≥ threshold** (default 10%), with
  **best-of-N auto-confirm** (re-measure a tripped benchmark 4× and fail only on a
  majority) rejecting transient outliers before failing.
- Runs on **both Linux and Windows** as two separate pipeline definitions (numbers are
  not cross-comparable).
- End-to-end (real SQL Server) only.
- **Out of scope:** cross-driver spec-compliant JSON (P50/P95/P99), mock-TDS
  microbenchmarks, pooled-connection scenario, Always Encrypted / Entra ID auth.

---

## 6. Future Considerations

### Dependency / toolchain regressions (longitudinal trend)

The pinned-commit isolation deliberately holds `Cargo.lock` and `rust-toolchain.toml`
constant, so it will **not** catch a regression introduced by a dependency bump (e.g.,
`tokio`, `native-tls`) or a compiler upgrade. These are two different questions:

| Question | What varies | What's held constant | Mode |
|----------|-------------|----------------------|------|
| "Did *my code* regress?" | mssql-tds source only | deps, toolchain, harness | Isolated (pinned-commit baseline) |
| "Did the *shipped artifact* get slower for any reason?" | code + deps + toolchain | nothing | Longitudinal trend of main |

A future **longitudinal trend mode** would track the tip of `main` over time (nothing
held constant), store results across builds, and watch for drift — catching slow creep
from dependency/toolchain changes that the isolated mode hides. Three modes in total:
(1) existing PR-vs-target, (2) new isolated pinned-commit baseline, (3) trend of main.

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

# Perf-lab monitoring runbook

How the weekly perf-lab runs are triaged, and when the baseline is advanced.

## The pipelines

| Platform | Definition | Schedule (UTC) | Typical duration |
|----------|-----------:|----------------|------------------|
| Linux    | [2294](https://sqlclientdrivers.visualstudio.com/mssql-rs/_build?definitionId=2294) | Mon 08:00 | ~1h |
| Windows  | [2298](https://sqlclientdrivers.visualstudio.com/mssql-rs/_build?definitionId=2298) | Mon 10:00 | ~1h40m |

Both run against `refs/heads/main` of the ADO mirror, which can lag GitHub `main`
by a few commits. Both compare that commit against the single SHA in
[`baseline-commit.txt`](baseline-commit.txt), shared by the two platforms.

## Reading a run

`run-benchmarks.sh` / `.ps1` echo the generated `summary.md` into the "Run tests
on perf VM" step log, fenced by `===== summary.md =====` markers, so triage needs
no artifact download. The same file is also published in the `perf-results`
artifact and rendered on the run's Summary tab.

The gate is already noise-hardened: a benchmark slower than the threshold
(`BENCH_REGRESSION_RATIO`, default `1.05` = 5%) trips it, is then re-measured 4×
interleaved, and is only confirmed when it trips in ≥3 of those 4 re-runs.
Apparent *improvements* qualify for the same treatment at the same magnitude
(`BENCH_IMPROVEMENT_VERIFY_RATIO` defaults to the regression threshold), but only
the largest `BENCH_IMPROVEMENT_VERIFY_MAX` of them (default 3) are actually
re-measured; any beyond that cap keep unverified first-pass numbers, and the
summary reports how many it skipped. Treat only a *reproduced* win as real: one
that was re-measured and did not reproduce is an artifact, and one that fell
outside the cap is simply unverified. Within a single run, take the verdict line
at face value: do not re-litigate a benchmark it cleared from the first-pass
numbers.

The Windows step log mangles the summary's UTF-8 (emoji, `±`, `µ`). The raw
critcmp block is still readable, and the emoji bars are a pure function of Δ%, so
the table can be reconstructed from it when quoting Windows results elsewhere.

## What the quorum does not cover

The 4 re-runs are interleaved inside a *single* VM session, so they establish
that a benchmark moved **on that machine, on that run**. A run-level condition —
host hardware, CPU frequency and thermal state, a noisy neighbor, SQL Server
state — biases all 4 equally, and the benchmark it lands on reproduces 4/4 and is
published as confirmed. This applies in **both** directions: the same re-measure
pass covers regressions and large apparent improvements (one `VERIFY_IDS` set,
one loop), so a session that happens to favor the candidate manufactures a
*verified improvement* just as readily as an unlucky one manufactures a
regression.

This is not hypothetical. On 2026-08-31, builds
[171113](https://sqlclientdrivers.visualstudio.com/mssql-rs/_build/results?buildId=171113)
and
[171243](https://sqlclientdrivers.visualstudio.com/mssql-rs/_build/results?buildId=171243)
confirmed disjoint sets of regressions with no `mssql-tds` source change between
them: `select_n_rows/10000` (+8.5%, 3/4, worst re-run +12%) and `temporal/decode`
(+6.0%, 4/4, worst +10%) in the first, `primitives/decode` (+8.0%, 4/4, worst
+11%) in the second, each clearing in the other run. Both runs measure the same
baseline commit, and their `base` columns — a repeated measurement of identical
code — differ by up to 5.2%, which is the gate threshold itself.

So the quorum rules out per-sample noise, not per-run noise, and a single
confirmed result is a *candidate* finding rather than a settled one. Until
[#434](https://github.com/microsoft/mssql-rs/issues/434) narrows the variance,
compare the two runs' baseline columns before trusting a verdict: when unchanged
baseline code moves by an amount comparable to the reported change, the run
is not decisive.

## Triage

- **Confirmed regression** (the completed summary verdict reports confirmed
  regressions) — re-queue the same definition once at the same commit and compare
  the two verdicts before reporting a regression or naming suspect commits. A
  benchmark that confirms in both runs is real; one that clears in the second was
  run-level noise the quorum could not see, and the pair of runs should be
  reported as inconclusive instead. This costs one run and is far cheaper than
  the bisect a false confirmation invites. Once a regression survives the second
  run, report the confirmed benchmarks with their trip counts and worst ratios,
  plus the commit range since the last green run on that platform.
- **Infra or harness failure** (no completed summary verdict, or the failing
  phase is VM deploy, SQL setup, toolchain/critcmp install, baseline SHA
  validation, missing bench binaries, or invalid `BENCH_*` settings) — re-queue
  only if the phase looks transient; otherwise fix the code or config. Escalate
  if a retry also fails.
- **Green** — check for verified improvements and for quorum-cleared drift, then
  apply the lock-in rules below. A verified improvement from a single run is a
  candidate too, not a settled win; criterion 3 says what it takes to spend one.

## Advancing the baseline

Advancing makes the candidate's numbers the new floor: a later change that gives
the gains back trips the gate instead of silently settling at the old level. That
cuts both ways — a bump that carries an unaddressed slowdown legitimizes it, and
the next gate is then measured from the degraded point.

This is why a false *improvement* is the more expensive error. A false regression
is self-correcting: someone investigates, finds nothing, and moves on. A false
improvement is self-perpetuating — it advances the floor to a number that was
never real, and every later run is then measured against that number, so the
noise is converted into a permanent regression for whoever comes next. The
criteria below are therefore stricter about the win than about the run.

Bump only when **all** of the following hold:

1. Both platforms are green at the **same** commit.
2. That commit is an ancestor of GitHub `main`.
3. At least one *verified* improvement (reproduced in ≥ quorum) on either
   platform, **reproduced again in a separate run on a fresh VM**. The
   improvement-verification re-runs share the single VM session with the
   regression re-runs — one `VERIFY_IDS` set, one loop — so they carry exactly
   the limitation described above, and a session biased toward the candidate
   yields a 4/4 "verified" win from nothing. One session is not evidence for a
   change that becomes the floor; a win that survives two sessions is.
4. No benchmark on **either** platform is more than **5%** slower than baseline.
   This is the gate's own magnitude, but applied with no quorum: a benchmark that
   trips in only 1–2 of the 4 re-runs is cleared by the gate, yet its published
   Δ% (the median of those re-runs) can still sit above 5%. Criterion 4 declines
   to bake that drift into the floor, so it is investigated rather than absorbed.

Criteria 3 and 4 are usually satisfiable from the same pair of runs, since
criterion 3 already calls for a second one — read both from the confirming run.

When (1)–(3) hold but (4) does not, report the win and the drift together; the
drift is the finding, and the bump waits for it to be explained or fixed. Note
that run-to-run variance is itself a common explanation for a criterion-4
slowdown — check the baseline columns across runs before treating one as a real
change.

The bump is a pull request editing `baseline-commit.txt` — reviewed, recorded in
git history, and attributable. See
[#346](https://github.com/microsoft/mssql-rs/pull/346) for the expected shape:
both platform tables inline, links to the two builds, and the outgoing → incoming
SHA in the description.

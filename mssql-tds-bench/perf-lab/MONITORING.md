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

The gate is already noise-hardened: a benchmark ≥10% slower trips it, is then
re-measured 4× interleaved, and is only confirmed when it trips in ≥3 of those 4
re-runs. Apparent *improvements* ≥10% get the same treatment, so an unreproduced
win is reported as an artifact rather than a gain. Take the verdict line at face
value; do not re-litigate a cleared benchmark from the first-pass numbers.

The Windows step log mangles the summary's UTF-8 (emoji, `±`, `µ`). The raw
critcmp block is still readable, and the emoji bars are a pure function of Δ%, so
the table can be reconstructed from it when quoting Windows results elsewhere.

## Triage

- **Confirmed regression** (the completed summary verdict reports confirmed
  regressions) — report the confirmed benchmarks with their trip counts and worst
  ratios, plus the commit range since the last green run on that platform. Do not
  retry: the quorum has already ruled out noise.
- **Infra or harness failure** (no completed summary verdict, or the failing
  phase is VM deploy, SQL setup, toolchain/critcmp install, baseline SHA
  validation, missing bench binaries, or invalid `BENCH_*` settings) — re-queue
  only if the phase looks transient; otherwise fix the code or config. Escalate
  if a retry also fails.
- **Green** — check for verified improvements and for sub-gate drift, then apply
  the lock-in rules below.

## Advancing the baseline

Advancing makes the candidate's numbers the new floor: a later change that gives
the gains back trips the gate instead of silently settling at the old level. That
cuts both ways — a bump that carries an unaddressed slowdown legitimizes it, and
the next 10% gate is then measured from the degraded point.

Bump only when **all** of the following hold:

1. Both platforms are green at the **same** commit.
2. That commit is an ancestor of GitHub `main`.
3. At least one *verified* improvement (reproduced in ≥ quorum) on either platform.
4. No benchmark on **either** platform is more than **5%** slower than baseline —
   tighter than the 10% gate, so drift is investigated rather than absorbed.

When (1)–(3) hold but (4) does not, report the win and the drift together; the
drift is the finding, and the bump waits for it to be explained or fixed.

The bump is a pull request editing `baseline-commit.txt` — reviewed, recorded in
git history, and attributable. See
[#346](https://github.com/microsoft/mssql-rs/pull/346) for the expected shape:
both platform tables inline, links to the two builds, and the outgoing → incoming
SHA in the description.

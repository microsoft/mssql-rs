---
name: perf-lab-triage
description: Triage the weekly mssql-tds perf-lab regression runs (Azure DevOps definitions 2294 Linux / 2298 Windows) and advance mssql-tds-bench/perf-lab/baseline-commit.txt when they qualify. Use when asked to check, triage or report on the perf-lab runs, the weekly performance regression pipelines, a perf gate verdict, or whether the performance baseline should move.
---

# Perf-lab triage

Weekly: read both platforms' regression runs, classify each, and move the shared
baseline when — and only when — the runs qualify.

## Scope

This covers the **mssql-tds** perf-lab: `mssql-tds-bench/perf-lab/`, ADO definitions
2294 and 2298, and the baseline at `mssql-tds-bench/perf-lab/baseline-commit.txt`.

`mssql-odbc-bench/perf-lab/` is a **separate, active** lab: its own
`baseline-commit.txt`, `msodbcsql-version.txt`, run scripts, and its own pipelines
(`.pipeline/odbc-perf-baseline-{linux,windows}-pipeline.yml`). Its baseline is
advanced by its own PRs. As of 2026-09 it has no in-tree runbook, so this skill covers
none of it — the definition IDs, the baseline path and the criteria here are all
mssql-tds's.

Do not read an mssql-tds verdict as saying anything about the ODBC baseline, and never
edit the ODBC baseline from this procedure. If you are asked to triage "the perf lab",
establish which one before touching anything.

## Invocation

Triage is scheduled outside the repo — a Copilot automation, a cron, or a human
asking. The schedule lives wherever it is configured, but the *prompt* does not need
to: keeping it here means the weekly run is reproducible from a checkout instead of
from one person's laptop.

The canonical weekly prompt is one line, because everything it used to carry is now in
this file:

> Triage this week's mssql-tds perf-lab runs per the `perf-lab-triage` skill, and open
> the baseline-advance PR if they qualify. Report back concisely.

Anything beyond that in an invoking prompt is either a deliberate override for that
run (say so explicitly, and remember the runbook still wins on policy) or content that
has drifted out of this file and should be folded back in.

**Prerequisite, and the one thing the repo cannot supply:** the run needs a host
already authenticated to Azure DevOps — `az account get-access-token` must succeed
non-interactively. An unattended run has nobody to answer a browser prompt, and will
otherwise report itself healthy while it hangs. Verify auth before the first API call
and fail loudly if it is missing.

## The runbook is authoritative

[`mssql-tds-bench/perf-lab/MONITORING.md`](../../../mssql-tds-bench/perf-lab/MONITORING.md)
owns the *policy*: what each verdict means, how the noise-hardening works, and the
four criteria for advancing the baseline. **Read it at the start of every run, from an
up-to-date checkout** — `git fetch origin main` first, and read it at `origin/main`
rather than whatever the working branch has. A long-lived worktree can sit dozens of
commits behind, and this policy is revised in response to real incidents: triaging
against a stale copy of it silently applies retired rules. (Observed 2026-09-28: a
worktree 47 commits behind still carried a version predating the run-to-run variance
findings, which had already changed both the regression procedure and criterion 3.)

This file deliberately does not restate those criteria — two copies of a rule is how
the rule rots. Cite them by number (criterion 3, criterion 4) and let the runbook say
what they are.

This file owns the *mechanics*: which pipelines, how to get a verdict out of a log
cheaply, and how the bump PR is shaped.

If the two disagree, or an invoking prompt disagrees with the runbook, the runbook
wins and the divergence is itself a finding — report it, and fix it with a PR against
whichever file is stale.

Every command, ID and observation below is dated, not permanent. Prefer the command
that re-derives a fact over the value written here; when what you observe contradicts
this file, trust the observation and report the drift.

## 1. Find the runs

Org `https://sqlclientdrivers.visualstudio.com/`, project `mssql-rs`. Definitions:
**2294** Linux (Mon 08:00 UTC), **2298** Windows (Mon 10:00 UTC). Both run the ADO
mirror's `refs/heads/main`, which can lag GitHub `main` by a few commits.

An Azure DevOps MCP server may or may not be exposed in a given session. When it
isn't, the `az` CLI reaches the same REST API and needs no interactive login on an
already-authenticated host (verified 2026-09-14):

```powershell
az pipelines runs list --org https://sqlclientdrivers.visualstudio.com/ --project mssql-rs `
  --pipeline-ids 2294 --branch refs/heads/main --top 5 `
  --query "[].{id:id,status:status,result:result,sha:sourceVersion,finish:finishTime}" -o json
```

Take the most recent **completed** run per definition and record buildId, result,
`sourceVersion` and finishTime. If the newest run is still in progress, wait for it
rather than triaging the one behind it — a stale verdict reported as this week's is
worse than a late one. Re-check every 10 minutes for up to 2 hours, then report it as
still running.

## 2. Read the verdict out of the step log

`run-benchmarks.sh` / `.ps1` echo the generated `summary.md` into the "Run tests on
perf VM" step log between `===== summary.md =====` and `===== end summary.md =====`.
That is the whole verdict. **Do not download the `perf-results` artifact** — the log
carries the same content at a fraction of the cost.

Find the step log by size rather than by a hardcoded id. It was id `22` on both
platforms in Sep 2026, but that tracks the step layout, not the log:

```powershell
$tok = az account get-access-token --resource 499b84ac-1321-427f-aa17-267ca6975798 --query accessToken -o tsv
$b   = 175044   # buildId
$base = "https://sqlclientdrivers.visualstudio.com/mssql-rs/_apis/build/builds/$b/logs"
$logs = Invoke-RestMethod -Uri "$base`?api-version=7.1" -Headers @{Authorization="Bearer $tok"}
$logs.value | Sort-Object lineCount -Descending | Select-Object -First 5 -Property id,lineCount
```

`499b84ac-1321-427f-aa17-267ca6975798` is the Azure DevOps resource ID; the token is
scoped to it. Then pull only the tail — the summary block sits a few hundred lines
before the end of the step, so ~500 lines is plenty:

```powershell
Invoke-WebRequest -Uri "$base/22?api-version=7.1&startLine=4600&endLine=5200" `
  -Headers @{Authorization="Bearer $tok"} -OutFile "$env:TEMP\perf-$b.txt"
```

**`az rest` cannot fetch these logs.** It decodes the body to a cp1252 stdout and dies
on the summary's `→` with `UnicodeEncodeError: 'charmap' codec can't encode character
'\u2192'`; `--output-file` does not save you, because it raises before writing. Use
`Invoke-WebRequest` with the bearer token as above. Keep the `&` inside the quoted URL
or PowerShell treats it as an operator.

From each summary capture: the verdict line, every benchmark's Δ%, the
confirmed-regression list with trip counts, and the improvement list with reproduction
counts. The `>>>` trailer lines printed just after `===== end summary.md =====`
restate the outcome in one line each and are a cheap cross-check:

```
>>> Auto-confirm cleared all 2 initial regression(s) as transient (none tripped in >= 3/4); passing.
>>> NOTE: apparent improvement in 'lob/1048576' did not reproduce (0/4); reported as a measurement artifact, not a real gain.
```

### The Windows log mangles UTF-8

Emoji, `±` and `µ` all arrive as `?`, and the Δ% column header renders as `�%`. The
numbers are intact in the **raw critcmp block** — read Windows values there, never off
the emoji table. The bars are a pure function of Δ%, so the table can be rebuilt when
quoting Windows results elsewhere: 🟩 faster, 🟥 slower, one square per ~1%, drawn
only for |Δ| ≥ 1%, capped at 12. Confirm that legend against the Linux run's own
(uncorrupted) legend line rather than trusting this paragraph.

## 3. Classify

The runbook's "Triage" section defines the categories and which ones justify a
re-queue. One mechanic it assumes but does not spell out:

**Do not key off the ADO build result.** A confirmed regression makes the harness exit
1 — but so do toolchain/critcmp install failures, baseline SHA validation, missing
bench binaries and invalid `BENCH_*` settings, several of which occur inside the same
test step. `result: failed` therefore distinguishes nothing on its own, and
`result: succeeded` is only meaningful once you have seen the verdict that produced
it. The reliable signal is whether a **completed summary verdict exists in the log**;
read that first, then apply the runbook's categories to what the failing phase was.

**A confirmed regression is a candidate finding, not a settled one**, and the runbook
requires a second run at the same commit before you report it or name suspects. Read
"What the quorum does not cover" before triaging one: the 4 re-runs are interleaved
inside a single VM session, so they rule out per-sample noise but not per-run noise, a
condition that biases all 4 equally. The same applies to a "verified" improvement.
Comparing the two runs' **baseline columns** is the check that matters — that column
is a repeated measurement of identical code, and it has differed by up to 5.2%, which
is the gate threshold itself.

Within a single run, though, the verdict line stands: do not re-litigate a benchmark
it cleared from the first-pass numbers.

## 4. Baseline lock-in

Apply the runbook's four criteria. Two of them need a command rather than a reading:

**Criterion 2** — the candidate must be an ancestor of *GitHub* `main`, not of the ADO
mirror:

```powershell
git fetch origin main
git merge-base --is-ancestor <sha> origin/main   # exit 0 = ancestor
git rev-list --count <baseline-sha>..origin/main # how far behind the baseline now is
```

**Criterion 3** — "verified improvement" means *reproduced in ≥ quorum*, reported
under "Large improvements (verification)" as `Verified (reproduced in >= 3/4)`. Three
candidates listed at `0/4` is **not** a verified improvement; it is three artifacts,
and criterion 3 fails. Keep that distinct from a win that fell outside
`BENCH_IMPROVEMENT_VERIFY_MAX` (default 3) and was never re-measured — the summary
reports how many it skipped. Unverified is not the same as disproved, but neither one
satisfies criterion 3.

A 4/4 win in a *single* run does not satisfy it either. The runbook requires the
improvement to reproduce **in a separate run on a fresh VM**, because the improvement
re-runs share one VM session with the regression re-runs and a session biased toward
the candidate manufactures a "verified" win from nothing. Budget for that extra run:
qualifying on criterion 3 normally costs a re-queue. The runbook also says which run
to use for criterion 4 afterwards — re-read it there rather than assuming the newest
run supersedes the old one on both platforms.

When criteria 1-3 hold and 4 does not, do **not** open a PR: report the win and the
offending slowdowns together. Per the runbook, the drift is the finding.

When all four hold, see [baseline-pr.md](./baseline-pr.md).

## 5. Report

Short enough to read on a phone. Detail belongs in the PR, not the message.

- One line per platform: build link, verdict, biggest win, biggest slowdown.
- Any confirmed regression, infra failure or harness fault — and what you did about
  it. Say explicitly when you re-queued something.
- Whether the baseline advanced; if not, **which numbered criterion failed**.
- The current baseline SHA and how many commits behind `main` it now is.

Name the criterion that failed rather than narrating the whole evaluation — "criterion
3 failed, no verified improvement" is the finding; the other three passing is not
news.

## Reporting drift in this skill

Every ID, command and threshold here is a dated observation. When one turns out to be
wrong — a renumbered definition, a step log that moved, an `az` invocation that
started or stopped working — fix it with a PR against this file, or file an issue
labelled `skill:perf-lab-triage` (mirroring `skill:code-review`; create the label if
it does not exist yet).

Keep that separate from the week's triage report. A reader asking "did perf regress?"
should not have to read about the skill's own maintenance, and a stale command here is
not the perf owner's problem to decode.

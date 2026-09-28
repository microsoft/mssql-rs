# Opening the baseline-advance PR

Mechanics for the bump itself. Referenced from `SKILL.md`; read this only once all
four of the runbook's criteria hold.

## The change

Work in a fresh worktree off latest `origin/main` so the main checkout stays clean:

```powershell
git fetch origin main
git worktree add ../mssql-rs-perf-baseline -b david/perf-baseline-<short-sha> origin/main
```

Replace **only** the 40-character SHA line in
`mssql-tds-bench/perf-lab/baseline-commit.txt`. Leave the comment header intact — it
explains why the file exists and is read by people who arrive via a failing gate. The
file's own parser ignores `#` lines and blanks, so the header is free.

Confirm the diff is exactly one line before going further:

```powershell
git diff --stat   # 1 file changed, 1 insertion(+), 1 deletion(-)
```

## Title

```
perf(lab): advance baseline to <short-sha> to lock in <what improved>
```

Name the actual win, not the act of bumping. "to lock in the token-stream decode gains"
tells a reviewer what regressing would cost; "to advance the baseline" tells them
nothing they can't see in the diff.

## Body

Model it on [#346](https://github.com/microsoft/mssql-rs/pull/346). Fill out the repo
PR template (`.github/PULL_REQUEST_TEMPLATE.md`) and include:

- The outgoing → incoming SHA, both as short SHAs with the full SHA available.
- Links to both builds, by buildId.
- Both platforms' **Change vs baseline** table and raw critcmp block, inline.
- The verified improvement(s) with reproduction counts — this is the evidence for
  criterion 3, and it is the reason the PR exists.

Rebuild the **Windows** emoji table from its raw Δ% values; the log's copy is
corrupted (see SKILL.md). 🟩 faster, 🟥 slower, one square per ~1%, drawn only for
|Δ| ≥ 1%, capped at 12.

Strip mid-sentence line wrapping before posting so GitHub can wrap the text itself.
Wrapping is fine while previewing in chat.

### The linked-issue check

`.github/workflows/pr-linked-issue-check.yml` fails the PR when the description
contains neither a GitHub issue reference (`#123`, or a
`github.com/microsoft/mssql-rs/issues/123` URL) nor an ADO work item URL matching
`sqlclientdrivers.visualstudio.com/.../_workitems/edit/<ID>`. It reads the **body
only** — a reference in the title or a comment does not satisfy it.

Put the reference under `## Related Issues`. Link the issue or work item that actually
tracks the perf work being locked in; if none exists, file one rather than pointing at
something unrelated to satisfy a regex. The check is a proxy for traceability, and
defeating the proxy defeats the point.

### The checklist

`cargo bfmt` / `cargo bclippy` / `cargo btest` are in the template, and this PR
compiles nothing — it edits a `.txt`. Tick what you actually ran and say the change is
data-only; do not tick three boxes blind to make the template look satisfied. A
reviewer who sees all-green on a PR that built nothing learns to distrust the
checklist everywhere else.

The real validation for this PR is the *next* scheduled run, which measures against
the new floor. Say so in the description.

## Draft vs ready — a known divergence

`.github/instructions/pr-workflow.instructions.md` says PRs here start as drafts and
are marked ready only after validation passes. The perf-baseline procedure as
operated opens this one **ready for review**, requests a Copilot review, labels it
`ready for human review`, and does not merge.

The reasoning for the divergence is that all the evidence is gathered *before* the PR
is opened and no iteration is expected — so draft-first buys a round trip and nothing
else. That reasoning is plausible, but it is not written down anywhere authoritative,
and a one-line exception living only in a skill file is exactly the sort of thing that
drifts.

Follow the operating procedure (ready, not draft), and **flag the divergence** so it
gets reconciled: either the PR-workflow instructions grow an explicit carve-out for
data-only automated PRs, or this procedure moves to draft-first. Do not quietly pick
one.

Verify the label exists before relying on it (`gh label list`), and fall back to
leaving it off rather than creating a new label mid-run.

## Merge

The author owns the merge. Open it, get it reviewed, and stop — an automated run never
merges its own baseline bump. If it needs to land before approvals complete, that is
the author's call to make via auto-merge, not the run's.

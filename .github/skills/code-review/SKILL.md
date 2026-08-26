---
name: code-review
description: Review a pull request, diff, or set of proposed changes in microsoft/mssql-rs — from a GitHub PR link, a PR number, or local staged/unstaged changes. Use whenever the user asks to review a PR, asks for feedback on a diff, or asks whether changes are ready to merge. Covers correctness, security, tests, readability, performance, API/breaking changes, and mssql-rs repo conventions.
---

# Pull Request Review

You are reviewing proposed changes. Review only what the diff changes plus directly
affected code — do not critique pre-existing code outside the PR's scope.

Every concrete number, constant, and known-failure list below is a dated observation,
not a standing truth. Prefer the command that re-derives a fact over the value written
here. If what you observe contradicts this file, trust the observation and report the
drift separately — an issue or PR against this skill, not the summary of whatever PR
you happen to be reviewing. Skill maintenance is not that author's problem.

## Process

1. Read the PR title/description to understand intent. Flag if the description is
   missing or doesn't match the diff. This repo requires a linked GitHub issue or
   Azure DevOps work item — flag a PR that has neither.
2. **Check the PR out locally.** A diff alone is not enough to review this codebase —
   most defects here turn on unchanged code (the other implementer of a trait, the
   caller three layers up, the `#[cfg]` variant of a constant). Use a dedicated
   worktree so the main worktree stays clean, and diff against the merge base rather
   than trusting per-commit stats: PRs here are commonly stacked and often carry a
   merge from `main`.

   ```bash
   gh pr view <url-or-number> --json title,body,author,state,isDraft,baseRefName,headRefName,additions,deletions,changedFiles,commits
   git fetch origin main                       # `origin/main` below must be current
   git fetch origin pull/<N>/head:pr<N>-review
   git worktree add ../mssql-rs-pr<N>-review pr<N>-review
   cd ../mssql-rs-pr<N>-review
   BASE=$(git merge-base origin/main HEAD)
   git diff --stat $BASE..HEAD
   ```

3. **Read what has already been said before writing anything.** PRs here routinely go
   through several rounds of author self-review plus a Copilot bot review, and many
   obvious findings are already raised, verified, and answered. Re-filing an answered
   thread as a new finding — especially at a higher severity — wastes the author's
   time and misranks the review.

   Prior discussion is split across three endpoints and you need all three. Listing
   reviewer *names* is not reading the reviews — a PR reporting nine reviews tells
   you nothing about what any of them said, and treating that silence as novelty is
   how an answered finding gets re-filed as blocking. Save the responses; the symbol
   search below needs something to search.

   ```bash
   R=repos/microsoft/mssql-rs
   gh api --paginate $R/pulls/<N>/comments \
     -q '.[] | "--- \(.user.login) \(.path):\(.line)\n\(.body)"' > /tmp/pr<N>-inline.txt
   gh api --paginate $R/pulls/<N>/reviews \
     -q '.[] | "--- \(.user.login) \(.state)\n\(.body)"'          > /tmp/pr<N>-reviews.txt
   gh api --paginate $R/issues/<N>/comments \
     -q '.[] | "--- \(.user.login)\n\(.body)"'                    > /tmp/pr<N>-toplevel.txt
   gh pr checks <N>
   ```

   The review *body* is the easiest slot to miss and often the most important:
   Copilot's low-confidence findings are **suppressed**, appearing only there inside a
   collapsed `<details>` block, never as an inline comment. An author's rebuttal to
   one is usually a PR-level comment. Read only the inline threads and both halves of
   that exchange are invisible.

   Then, before drafting each finding, grep those files for the symbol it concerns.
   One search for a name like `mark_known_dead` surfaces the thread that already
   settled it, far cheaper than re-reading the whole history.

   *If* an automated **Code Coverage Report** comment is present, read it before
   writing any coverage finding; its absence is usually expected rather than a CI
   failure. Both under "Verify Before You Claim".
4. Verify claims against the actual code — do not assume. Read surrounding code when
   a change's correctness depends on context: callers of changed functions,
   implementers of changed traits, and the layer above and below the change.
5. **Run tests to answer a question, not to re-collect a verdict.** CI runs the suite
   and the merge is gated on it, so `gh pr checks` from step 3 already tells you pass
   or fail — across platforms and cross-repo suites you cannot reproduce locally.
   Rebuilding the workspace to learn the same thing costs minutes and returns less.

   Build and run when you need something the CI result cannot give you:

   - **Does this new test actually guard the change?** Mutate the constant, operator
     or condition the PR changed and confirm the test fails. This is what catches
     vacuous tests and is the highest-value reason to build locally.
   - **Is this failure the PR's, or pre-existing?** Run the *same invocation* on
     `$BASE` and compare failure sets; only the difference belongs in the review.
   - **Is this coverage gap real?** Introduce the bug the missing test would catch and
     show the suite still passes.

   ```bash
   cargo nextest run -p <affected-crate> --lib --no-fail-fast   # or `cargo btest`
   ```

   The runner is `cargo nextest` / `cargo btest`, never `cargo test`. Pick the crate
   and targets the diff touches — `-p mssql-tds --lib` validates nothing in a PR
   confined to `mssql-odbc`, the JS or Python bindings, or the e2e suites. Feature
   selection changes both the test count and which tests fail, so never compare a
   `--all-features` run against a default one. Some tests fail on a clean tree because
   their fixtures aren't generated locally (historically `certificate_validator`
   looking for `tests/test_certificates/*.pem`) — which is why the baseline matters.
6. **Present the review in chat and wait for explicit human confirmation before
   posting anything to GitHub.** Inline comments are drafted against
   `file:line`, not submitted, until they say so. Having the review fully written and
   the posting mechanics ready is not permission to post.

   **Automated runs.** Skip the confirmation only when posting without it has been
   explicitly authorized — an instruction in the invoking prompt, or a pipeline
   configured to post. Inferring "this looks like an automated context" is not
   authorization; when it is ambiguous, ask. An unattended run still:

   - posts `event: "COMMENT"` only. Never `APPROVE` or `REQUEST_CHANGES` — an
     automated approval can satisfy branch protection, which makes it a governance
     problem rather than a review one.
   - notes in the body that it came from an unattended run, so the author knows the
     findings were not checked by a human first.
   - never merges, and never resolves a thread it did not open.
7. Ground yourself in reference code and public/private documentation/specifications.
   If you don't know the codebase, or which references to use, ask for context before
   reviewing.

## What to Check

Correctness, security, tests, readability, performance and API surface all apply as
usual — skip areas that don't apply rather than padding the review. What is worth
stating here is only what is specific to this repo, or easy to get wrong in it:

- **Tests** are the highest-yield area. "Has a test" is not the question; "does the
  test fail when the change is reverted" is.
- **Performance** on the row-decode path needs a timing, never a byte table.
- **API & breaking changes** include the FFI surface — `#[napi]`, `#[pyclass]`,
  `extern "C"` — not just Rust signatures, and "breaking" needs a publication check.
- **Repo conventions** live in `.github/copilot-instructions.md`,
  `.github/instructions/*.md` and any `AGENTS.md`. Cite one before flagging a
  convention.

All four are expanded under "Verify Before You Claim", which is where the evidence is.

## mssql-rs Specifics

Check these in addition to the general areas above.

- **License header**: every new `.rs` file starts with the Microsoft copyright and
  MIT license header.
- **Protocol layering**: changes respect Transport → IO → Token stream → Message →
  Client API. Flag a layer reaching past its neighbor.
- **Module layout**: `foo.rs` declares `pub mod` items with implementations under
  `foo/`.
- **Errors**: `thiserror` derives and `TdsResult<T>`; no `unwrap`/`expect`/`panic!`
  on paths reachable from user input or network data.
- **Async**: no blocking work on the Tokio runtime; cancellation flows through
  `CancelHandle`; box new non-primitive fields in long-lived client-context structs
  when doing so keeps async state smaller.
- **Visibility**: new items are `pub(crate)` unless a public surface is intended.
- **Naming**: `Tds` prefix on core public types.
- **Unsafe code**: any new `unsafe` block — especially in `mssql-odbc` FFI — has a
  justification and upholds the invariants it assumes.
- **ODBC attribute symmetry**: an attribute added to a setter needs the matching
  getter arm, and the get side should answer what the set side accepts. Three PRs
  shipped `SQL_SUCCESS` to set and `HY092` to read back the same attribute.
- **Tests**: unit tests in inline `#[cfg(test)]` modules for pure logic, integration
  tests under `tests/`. Reuse existing fixtures and env helpers (`conftest.py` for
  Python) rather than inventing new patterns. Prefer `mssql-mock-tds` over requiring
  a live server.
- **Excluded crate**: `mssql-py-core` is outside the workspace — if it changed,
  confirm fmt/clippy were run against it separately.
- **Validation**: the PR checklist claims `cargo bfmt`, `cargo bclippy`, and
  `cargo btest` pass. Flag a checked box that the CI run contradicts.
- **No AI slop**: no comments restating what the code does, no filler phrases, no
  redundant validation or duplicated logic.

## Verify Before You Claim

These are findings that have been filed against this repo and turned out to be wrong.
Each one costs a retraction, so check the stated source before raising that class.
The measurements below are evidence for the rule, not current state — they explain why
the rule exists and do not need re-deriving. Last reviewed 2026-08.

- **Parity findings in `mssql-odbc`: msodbcsql is the contract, the ODBC spec is
  not.** This is the single largest source of withdrawn findings here. Reviews have
  argued from the spec or from internal consistency and been overturned by the
  reference driver in both directions — a proposed `07006`→`07009` correction where
  msodbcsql reports nothing at all; "write the non-NULL length too" where msodbcsql
  writes nothing; "gate these renames on ODBC 2.x" where `DoDD()` applies them
  unconditionally; "add the missing post-connect guard" where msodbcsql deliberately
  has none. It also cuts the other way: `BufferLength = 0` *is* a length probe there,
  and raising that as a question found a real divergence.
  - Cite the msodbcsql file and line, or ask as a question. Never assert parity from
    the spec.
  - **Read the caller, not just the table or validator.** msodbcsql normalizes on
    entry, so a validator read in isolation misleads. One finding called an `HYC00`
    arm reachable after reading the validator alone; `SQLBindParameter` folds
    `SQL_DOUBLE` to `SQL_FLOAT` *before* calling it, which is what makes that arm
    dead. Retracted by its own author.
- **A test that still passes when you break the thing it names guards nothing.** The
  most common defect in this repo's *tests*, and the cheapest to check: mutate the
  constant, operator or condition the change is about and confirm the test fails.
  Real examples — a limb-reassembly test where `<< (i * 32)` → `<< (i * 16)` left all
  539 tests green; temporal tests built in nanoseconds and asserted against a
  converter that divided by 1e9, self-consistently wrong while the driver ran 100x
  off; a test that passed on `Err(ConnectionClosed)` rather than the behavior in its
  name; an e2e case that passed with *and* without the guard it was added for; a
  redaction test whose secret rendered as `[171, 171, 171, 171]` and was never
  asserted against. Also check whether it passes on `$BASE` — a cursor the fix was
  meant to sweep had already been swept by the setup.
- **Coverage**: when the automated "Code Coverage Report" comment is present, read it
  rather than computing your own. A local `cargo llvm-cov --lib` badly understates the
  CI number for `mssql-odbc`, because CI merges the cross-repo `mssql-python` suite
  into it — one PR measured 83.9% locally and 97% in CI. The report also says diff
  coverage is *reported, not enforced*.
  - **Its absence is usually expected, not a CI failure.** `pr-code-coverage.yml`
    mirrors the ADO path excludes, so a PR touching only `.github/**`, `docs/**`,
    `README.md`, `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`, `LICENSE` or
    `es-metadata.yml` never generates one; fork PRs need a maintainer to comment
    `/coverage`. Read the workflow's `paths-ignore` before reporting a missing report.
- **Coverage gaps**: don't assert one, prove it. Introduce the bug the missing test
  would catch, run the affected crate's suite, show it still passes, then restore.
  One incremental build and a suite run converts a concern into a fact.
- **"This is a breaking API change"**: check that anything outside the workspace could
  observe it. `mssql-tds` is unpublished and pre-1.0, and the sibling crates build
  `ClientContext` through `Default`/`From` rather than struct literals, so a field
  addition broke nothing. Verify with `cargo clippy --workspace --all-features
  --all-targets` plus a `crates.io` check before calling it breaking.
- **Unreachable branches**: worth flagging, together with any test that asserts the
  tautology — but both dispositions are legitimate. Deleting is right when the check
  reads as real validation; keeping it with a note is right when removal turns a
  future drift into a panic across the FFI boundary. Ask, don't demand.
- **Perf on the row-decode path**: if the claim is about time, cite a timing. Size and
  time have measured *anti-correlated* here: cancellation plumbing came to ~70% of the
  row future by size but +5.8% by time, while a per-row `tokio::time::timeout` cost
  +112 B but +42.5%. A prototype that shrank the future 32% benchmarked *slower*. Byte
  tables predict nothing on this path.
- **Future-size budgets**: the budget asserted by `row_fetch_futures_stay_small` is a
  ceiling, not a ratchet — read the current constant out of the test. Growth that
  stays under it is not a regression without a timing. Also, `TdsTokenStreamReader` /
  `TdsTransport` are `#[async_trait]`, so a caller future holds a pointer and bounds
  nothing inside the trait method — measure at the real call site.
- **Perf follow-ups**: check the issue tree first. Row-decode perf work is tracked
  under a parent issue with a sub-issue per axis, so a "new" finding is often already
  filed, with numbers attached. Find it rather than trusting a number written here:

  ```bash
  gh issue list --state all --search "row decode perf"
  ```
- **Repo conventions**: a real convention finding cites the file and line that
  mandates it. Verify against `.github/PULL_REQUEST_TEMPLATE.md`,
  `CONTRIBUTING.md` / `AGENTS.md` / `.github/copilot-instructions.md`, and actual
  behavior in `git log origin/main -25`. Otherwise it is a guess, and guesses go in
  the body as questions.
- **A CHANGELOG.md entry is not required** — it is in no checklist, and the only
  `.github/` reference is a `paths-ignore` that makes changelog-only edits *skip* CI.
  Nit at most, never a finding on its own. The related finding that *is* real: a
  behavior fix buried in a refactor or deletion PR should be named in the
  description, wherever it ends up recorded.
- **Removals in a stacked PR**: check the net `git diff $BASE..HEAD` before calling
  something a removal. Scaffolding added in commit 1 and deleted in commit 3 of the
  same PR is hygiene, not a defect — per-commit stats mislead.
- **"This PR introduces X"**: check whether the same shape already exists on `main`.
  A pre-existing, crate-wide gap — the raw-handle lifetime races are the standing
  example, already tracked by a TODO in `disconnect.rs` — is a legitimate deferral,
  because fixing it in one path and not the others is worse than scheduling it as its
  own change.

## Reviewing Alongside Other Reviewers

When handed findings from a bot or another agent, adjudicate rather than forward.

- Verify the mechanism against the code, then verify the *proposed fix* too. A remedy
  can be broken independently of the diagnosis being right.
- **Bot findings assert source and spec facts that often do not hold.** Check each
  one against the actual file. Withdrawn examples: `SQL_ATTR_CURSOR_TYPE` is
  `SQLULEN`, not the `SQLUINTEGER` the finding claimed; `ProcessRow` never reads the
  field it said to cache; `DoDD()` has no version gate. Their "this issue also
  appears at lines X, Y, Z" lists are worth checking individually — two PRs found
  those extra locations were unrelated code.
- Check whether the thread has already been raised and answered. A restatement at a
  higher severity is not a new finding, and the existing reply usually contains the
  reason the obvious fix was declined.
- Reframe severity when the mechanism is real but the impact argument is not. Say
  which part you kept and which part you corrected.

## Posting the Review

Only after explicit confirmation, or under the automation carve-out in step 6.
The mechanics that otherwise fail silently — inline comments needing the API rather
than `gh pr review`, diff-hunk anchoring, `--paginate` when verifying — are in
[posting.md](./posting.md).

## Output Format

1. **Summary** — 1-3 sentences: what the PR does and overall assessment. For new features, include what you referenced to verify correctness.
2. **Findings grouped by severity:**
   - **Blocking** — must fix before merge (bugs, security, breaking changes without
     handling).
   - **Suggestion** — should consider; improves quality but not merge-blocking.
   - **Nit** — minor/optional (style, naming, typos).
3. Each finding for a specific `file:line` gives a concrete fix or a focused code
   snippet — not just "this is wrong." Leave the comment at that line so it carries
   context and can be tracked to resolution.

## Principles

- **Question a departure from the reference drivers before auditing inside it.** When
  a change diverges from `msodbcsql` / `SqlClient` / `mssql-jdbc` behavior, the
  divergence is the first thing to examine — hardening a path that shouldn't exist is
  wasted review. If a PR's own design doc reaches opposite conclusions in two places,
  that contradiction *is* the finding.
- Distinguish facts (verified in code) from concerns (worth checking). Don't state
  guesses as defects. Say what you ran and what you read.
- If a change is correct, don't invent problems. An empty severity group means "none
  found" — say so briefly.
- Reviewing is not merging. The PR author owns the merge — never merge someone
  else's PR.

---
name: code-review
description: Review a pull request, diff, or set of proposed changes in microsoft/mssql-rs — from a GitHub or Azure DevOps PR link, a PR number, or local staged/unstaged changes. Use whenever the user asks to review a PR, asks for feedback on a diff, or asks whether changes are ready to merge. Covers correctness, security, tests, readability, performance, API/breaking changes, and mssql-rs repo conventions.
---

# Pull Request Review

You are reviewing proposed changes. Review only what the diff changes plus directly
affected code — do not critique pre-existing code outside the PR's scope.

## Process

1. Read the PR title/description to understand intent. Flag if the description is
   missing or doesn't match the diff. This repo requires a linked GitHub issue or
   Azure DevOps work item — flag a PR that has neither.
2. Review the full diff against the base branch before commenting.

   ```bash
   gh pr view <url-or-number> --json title,body,author,state,baseRefName,files,additions,deletions
   gh pr diff <url-or-number>
   ```

   For Azure DevOps repos, use the `repo_pull_request` tool with `action: get`.
3. Verify claims against the actual code — do not assume. Read surrounding code when
   a change's correctness depends on context: callers of changed functions,
   implementers of changed traits, and the layer above and below the change.
4. **Present the review in chat and wait for explicit human confirmation before posting anything to GitHub or ADO.** Inline comments are drafted against `file:line`, not submitted, until they say so.
5. Ground yourself in reference code and public/private documentation/specifications. If you don't know the codebase, or which references to use, ask for context before reviewing.
6. 

## What to Check

Evaluate each area. Skip areas that don't apply rather than padding the review.

- **Correctness**: Logic bugs, off-by-one, null/empty/boundary cases, error handling,
  race conditions, incorrect assumptions.
- **Security**: OWASP Top 10 — injection, broken auth/access control, SSRF,
  deserialization. Hardcoded secrets/credentials. Unvalidated input at trust
  boundaries. Unsafe defaults.
- **Tests**: New/changed behavior has tests. Tests assert real behavior (not
  tautologies). Edge cases and failure paths covered. Flag untested risky changes.
  Diff coverage should be at least 85% to match CI.
- **Readability & maintainability**: Clear naming, reasonable function size, no
  needless complexity or duplication, comments explain *why* not *what*.
- **Performance**: N+1 queries, lock contention and lock scope, memory copies,
  unnecessary allocations in hot paths, blocking I/O on async paths, O(n²) where
  avoidable, network round trips. Only raise when impact is plausible.
- **API & breaking changes**: Public signatures, serialization formats, config, and
  the FFI surface (`#[napi]`, `#[pyclass]`, `extern "C"`). Flag breaking changes and
  whether they're versioned/documented.
- **Repo conventions**: Match existing patterns, style, and structure in the
  codebase. Respect `.github/copilot-instructions.md` and any `AGENTS.md`.

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
  `CancelHandle`; boxed non-primitive fields in long-lived client-context structs to
  keep async state small.
- **Visibility**: new items are `pub(crate)` unless a public surface is intended.
- **Naming**: `Tds` prefix on core public types.
- **Unsafe code**: any new `unsafe` block — especially in `mssql-odbc` FFI — has a
  justification and upholds the invariants it assumes.
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

## Output Format

1. **Summary** — 1-3 sentences: what the PR does and overall assessment.
2. **Findings grouped by severity:**
   - **Blocking** — must fix before merge (bugs, security, breaking changes without
     handling).
   - **Suggestion** — should consider; improves quality but not merge-blocking.
   - **Nit** — minor/optional (style, naming, typos).
3. Each finding for a specific `file:line` gives a concrete fix or a focused code
   snippet — not just "this is wrong." Leave the comment at that line so it carries
   context and can be tracked to resolution.

## Principles

- Be specific and actionable; avoid vague praise or vague criticism.
- If a change is correct, don't invent problems. An empty severity group means "none
  found" — say so briefly.
- Distinguish facts (verified in code) from concerns (worth checking). Don't state
  guesses as defects.
- Prefer the smallest correct fix over large refactors unless the PR's goal requires
  more.
- Reviewing is not merging. The PR author owns the merge — never merge someone
  else's PR.

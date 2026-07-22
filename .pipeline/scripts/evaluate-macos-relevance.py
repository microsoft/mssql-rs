#!/usr/bin/env python3
"""macOS validation relevance guard.

Determines whether a pull request's changes can affect the macOS build/test jobs.
Only a subset of the workspace has macOS-specific behavior (Apple Security.framework
TLS/crypto and GSSAPI, plus dependency manifests and pipeline definitions). When a PR
touches none of those paths, the macOS jobs add wall-clock time without adding coverage,
so this guard emits ``macRelevant=false`` to let them short-circuit.

Any missing prerequisite (target branch, git failure) or unexpected error falls back to
``macRelevant=true`` so the macOS jobs run (safe default: never skip on uncertainty).

Consumed by the ``SetMacRelevance`` step of the ``EvaluateRequirements`` stage in
``.pipeline/templates/validation-stages.yml``. The macOS jobs also always run on non-PR
builds regardless of this variable (that gating lives in the pipeline conditions).
"""
import fnmatch
import os
import subprocess
import sys

# Paths whose changes require a macOS validation run (moderate allowlist).
# fnmatch-style globs matched against repo-relative POSIX paths. Note fnmatch's
# ``*`` also spans ``/``, so a trailing ``/*`` matches arbitrarily deep subtrees.
MAC_RELEVANT_GLOBS = [
    # macOS-specific security libraries: Security.framework/CommonCrypto crypto and TLS.
    "mssql-tds/src/security/*",
    "mssql-tds/src/connection/transport.rs",
    "mssql-tds/src/connection/transport/*",
    # macOS GSSAPI/Kerberos integration test.
    "mssql-tds/tests/test_kerberos_gssapi.rs",
    # Dependency graph / toolchain: a resolution change can break the macOS build.
    "*Cargo.toml",
    "Cargo.lock",
    "rust-toolchain*",
    # Pipeline definitions themselves (templates, scripts) — high-impact, run macOS.
    ".pipeline/*",
]


def set_mac_relevant(value):
    print(f"##vso[task.setvariable variable=macRelevant;isOutput=true]{value}")


def matches_allowlist(path):
    return any(fnmatch.fnmatch(path, pattern) for pattern in MAC_RELEVANT_GLOBS)


def main():
    build_reason = os.environ.get("BUILD_REASON", "")
    if build_reason != "PullRequest":
        # Non-PR builds always run macOS in full; the guard is a no-op here.
        print(
            f"Build.Reason='{build_reason or '<empty>'}' is not PullRequest; "
            "macOS jobs run in full."
        )
        set_mac_relevant("true")
        return

    target_branch = os.environ.get("SYSTEM_PULLREQUEST_TARGETBRANCH", "")
    if not target_branch:
        print(
            "##vso[task.logissue type=warning]System.PullRequest.TargetBranch is unavailable; "
            "running macOS validation."
        )
        set_mac_relevant("true")
        return

    # TargetBranch arrives as e.g. 'refs/heads/main' or 'main'; normalize to a ref
    # the local clone can resolve. ADO checks out the PR merge ref with the target
    # branch tip available as origin/<branch>.
    short_branch = target_branch
    for prefix in ("refs/heads/", "refs/remotes/origin/"):
        if short_branch.startswith(prefix):
            short_branch = short_branch[len(prefix):]
            break

    candidate_refs = [
        f"origin/{short_branch}",
        short_branch,
        target_branch,
    ]

    diff_output = None
    used_ref = None
    for ref in candidate_refs:
        try:
            # Three-dot diff against the merge base isolates the PR's own changes
            # from unrelated commits already on the target branch.
            diff_output = subprocess.check_output(
                ["git", "diff", "--name-only", f"{ref}...HEAD"],
                stderr=subprocess.STDOUT,
                text=True,
            )
            used_ref = ref
            break
        except subprocess.CalledProcessError:
            continue
        except OSError as exc:
            print(
                f"##vso[task.logissue type=warning]Unable to invoke git ({exc}); "
                "running macOS validation."
            )
            set_mac_relevant("true")
            return

    if diff_output is None:
        print(
            "##vso[task.logissue type=warning]Could not compute PR diff against target "
            f"branch '{target_branch}' (tried {candidate_refs}); running macOS validation."
        )
        set_mac_relevant("true")
        return

    changed_files = [line.strip() for line in diff_output.splitlines() if line.strip()]
    print(f"Comparing against '{used_ref}'; {len(changed_files)} changed file(s).")

    if not changed_files:
        # An empty diff is unexpected for a PR; do not skip on that ambiguity.
        print(
            "##vso[task.logissue type=warning]PR diff is empty; running macOS validation."
        )
        set_mac_relevant("true")
        return

    matched = [path for path in changed_files if matches_allowlist(path)]
    if matched:
        preview = ", ".join(matched[:10])
        suffix = "" if len(matched) <= 10 else f" (+{len(matched) - 10} more)"
        print(f"macOS-relevant change(s) detected: {preview}{suffix}")
        set_mac_relevant("true")
    else:
        print(
            "No macOS-relevant paths changed; skipping macOS validation jobs for this PR."
        )
        set_mac_relevant("false")


if __name__ == "__main__":
    # Safety net: the guard is best-effort, so any unexpected failure must fall back
    # to running macOS validation (macRelevant=true) rather than skipping coverage.
    try:
        main()
    except Exception as exc:  # noqa: BLE001 - deliberate catch-all for safe fallback
        print(
            f"##vso[task.logissue type=warning]macOS relevance guard failed unexpectedly "
            f"({type(exc).__name__}: {exc}); running macOS validation."
        )
        set_mac_relevant("true")
        sys.exit(0)

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Push version bumps and open a tracking issue with a manual PR link."""

import json
import os
import subprocess
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen

CRATES = ("mssql-tds", "mssql-mock-tds")
BUMP_BRANCH = "automation/bump-released-crate-versions"
ISSUE_MARKER = "<!-- mssql-rs:released-crate-version-bump -->"


def published_versions(crate):
    request = Request(
        f"https://crates.io/api/v1/crates/{crate}",
        headers={"User-Agent": "microsoft/mssql-rs scheduled version check"},
    )
    try:
        with urlopen(request, timeout=30) as response:
            data = json.load(response)
    except HTTPError as error:
        if error.code != 404:
            raise
        print(f"{crate}: not published on crates.io; leaving its version unchanged.")
        return set()
    versions = {entry["num"] for entry in data["versions"]}
    if not versions or not all(isinstance(version, str) for version in versions):
        raise ValueError(f"{crate}: invalid crates.io versions response")
    return versions


def cargo_versions(root):
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
        cwd=root, check=True, stdout=subprocess.PIPE, text=True,
    )
    return {
        package["name"]: package["version"]
        for package in json.loads(result.stdout)["packages"]
    }


def bump_versions(root, published):
    before = cargo_versions(root)
    selected = [crate for crate in CRATES if before[crate] in published[crate]]
    # The mock package has an exact, versioned dependency on mssql-tds. Do not
    # create a mock-only bump while that dependency version is unpublished:
    # Cargo drops the path when packaging, so the release cannot resolve it.
    if "mssql-mock-tds" in selected and "mssql-tds" not in selected:
        selected.remove("mssql-mock-tds")
        print("mssql-mock-tds: waiting for its mssql-tds dependency to be published.")
    if not selected:
        return {}
    subprocess.run(
        ["cargo", "set-version", "--bump", "minor"]
        + [arg for crate in selected for arg in ("--package", crate)],
        cwd=root, check=True,
    )
    after = cargo_versions(root)
    for crate in selected:
        if after[crate] in published[crate]:
            raise ValueError(f"{crate} {after[crate]} is already published; bump it manually.")
    return {crate: (before[crate], after[crate]) for crate in selected}


def push_bump_branch(root):
    def git(*args):
        return subprocess.run(
            ["git", *args], cwd=root, check=True, stdout=subprocess.PIPE, text=True,
        ).stdout.strip()

    ref = f"refs/heads/{BUMP_BRANCH}"
    remote = git("ls-remote", "--heads", "origin", ref)
    previous = remote.split()[0] if remote else ""
    git(
        "-c", "user.name=github-actions[bot]",
        "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com",
        "commit", "--only", "-m", "Bump released crates to the next minor version",
        "--", *(str(Path(crate) / "Cargo.toml") for crate in CRATES),
    )
    if previous:
        git("fetch", "--no-tags", "--depth=1", "origin", previous)
        if git("rev-parse", "HEAD^{tree}") == git("rev-parse", "FETCH_HEAD^{tree}"):
            print(f"{BUMP_BRANCH}: already up to date.")
            return
    # An explicit expected SHA also protects first creation from a concurrent push.
    git("push", f"--force-with-lease={ref}:{previous}", "origin", f"HEAD:{ref}")
    print(f"Updated {BUMP_BRANCH}.")


def ensure_bump_issue(summary, crates):
    endpoint = f"repos/{os.environ['GITHUB_REPOSITORY']}/issues"
    owner, name = os.environ["GITHUB_REPOSITORY"].split("/")
    # The REST listing can lag writes; use the direct issues connection, not search.
    query = """
    query($owner: String!, $name: String!, $endCursor: String) {
      repository(owner: $owner, name: $name) {
        issues(first: 100, after: $endCursor, states: OPEN) {
          nodes { number body }
          pageInfo { hasNextPage endCursor }
        }
      }
    }
    """
    result = subprocess.run(
        ["gh", "api", "graphql", "-f", f"query={query}", "-f", f"owner={owner}",
         "-f", f"name={name}", "--paginate", "--slurp"],
        check=True, stdout=subprocess.PIPE, text=True,
    )
    matches = [
        issue for page in json.loads(result.stdout)
        for issue in page["data"]["repository"]["issues"]["nodes"]
        if (issue.get("body") or "").startswith(ISSUE_MARKER)
    ]
    if len(matches) > 1:
        raise ValueError("Multiple open version bump tracking issues; resolve duplicates manually.")

    base = quote(os.environ["DEFAULT_BRANCH"], safe="")
    branch = quote(BUMP_BRANCH, safe="")
    compare = f"https://github.com/{os.environ['GITHUB_REPOSITORY']}/compare/{base}...{branch}?expand=1"
    body = (
        f"{ISSUE_MARKER}\n\n"
        "### Problem statement\n\n"
        "The default branch uses crate versions that are already published on crates.io.\n\n"
        "### Proposed solution\n\n"
        f"Start the next minor development versions:\n\n{summary}\n\n"
        f"The changes are prepared on `{BUMP_BRANCH}`. "
        "Check for an existing PR before choosing one of these options:\n\n"
        f"1. [Create PR]({compare}) yourself using the prepared branch.\n"
        "2. Assign this issue to Copilot through **Assignees** "
        "(if enabled for you and this repository). "
        f"Ask it to open a PR against `{os.environ['DEFAULT_BRANCH']}` "
        "using the prepared branch's changes. Do not bump the versions again.\n\n"
        "For either option, include `Fixes #<this issue number>` in the PR description.\n\n"
        "### Affected crate\n\n"
        + ", ".join(f"`{crate}`" for crate in crates)
        + "\n\n### Alternatives considered\n\nBump the versions manually.\n\n"
        "### Additional context\n\n"
        "Managed by the Bump Released Crate Versions workflow. "
        "Local versioned dependencies are kept in sync. "
        "This workflow does not create PRs, publish crates, or merge changes. "
        "Validation and review are required before merging.\n"
    )
    if matches and matches[0]["body"] == body:
        number = matches[0]["number"]
        print(f"Reusing version bump issue #{number}.")
        return number

    payload = {"body": body}
    if matches:
        endpoint += f"/{matches[0]['number']}"
        method = "PATCH"
    else:
        payload["title"] = "Bump released crates to the next minor version"
        method = "POST"
    result = subprocess.run(
        ["gh", "api", endpoint, "--method", method, "--input", "-"],
        input=json.dumps(payload), check=True, stdout=subprocess.PIPE, text=True,
    )
    number = json.loads(result.stdout)["number"]
    print(f"{'Updated' if matches else 'Created'} version bump issue #{number}.")
    return number


def main():
    root = Path(__file__).resolve().parents[1]
    # Fetch both before editing: a registry outage must not produce a partial bump.
    versions = {crate: published_versions(crate) for crate in CRATES}
    changes = bump_versions(root, versions)
    if changes:
        summary = "\n".join(
            f"- `{crate}`: `{current}` -> `{bumped}`"
            for crate, (current, bumped) in changes.items()
        )
        push_bump_branch(root)
        ensure_bump_issue(summary, changes)
        print(summary)
    else:
        print("No version bumps needed.")


if __name__ == "__main__":
    main()

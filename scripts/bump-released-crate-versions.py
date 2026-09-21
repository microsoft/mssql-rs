# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Create or update a tracking issue when source crate versions are already published."""

import json
import os
import subprocess
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

CRATES = ("mssql-tds", "mssql-mock-tds")
ISSUE_LABEL = "automation:crate-version-bump"
ISSUE_MARKER = "<!-- mssql-rs:released-crate-version-bump -->"


def published_versions(crate):
    for attempt in range(3):
        request = Request(
            f"https://crates.io/api/v1/crates/{crate}",
            headers={
                "User-Agent": "microsoft/mssql-rs version check (https://github.com/microsoft/mssql-rs)",
            },
        )
        try:
            with urlopen(request, timeout=30) as response:
                data = json.load(response)
            break
        except HTTPError as error:
            if error.code == 404:
                print(f"{crate}: not published on crates.io; leaving its version unchanged.")
                return set()
            if error.code in {429, 500, 502, 503, 504} and attempt < 2:
                time.sleep(2 ** attempt)
                continue
            raise
        except URLError as error:
            if attempt < 2:
                time.sleep(2 ** attempt)
                continue
            raise
    versions = {entry["num"] for entry in data["versions"]}
    if not versions or not all(isinstance(version, str) for version in versions):
        raise ValueError(f"{crate}: invalid crates.io versions response")
    return versions


def cargo_versions(root):
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=root, check=True, stdout=subprocess.PIPE, text=True,
    )
    current = {
        package["name"]: package["version"]
        for package in json.loads(result.stdout)["packages"]
    }
    missing = [crate for crate in CRATES if crate not in current]
    if missing:
        raise ValueError(f"{', '.join(missing)} not found in cargo metadata; update CRATES.")
    return current


def version_tuple(version):
    parts = version.split(".")
    if len(parts) != 3 or any(not part.isdigit() for part in parts):
        raise ValueError(
            f"{version} is not a valid three-part release version; choose the next version manually."
        )
    return tuple(int(part) for part in parts)


def next_minor(version):
    major, minor, _patch = version_tuple(version)
    return f"{major}.{minor + 1}.0"


def summary_line(crate, old, new):
    return f"- `{crate}`: `{old}` -> `{new}`"


def planned_bumps(root, published):
    current = cargo_versions(root)
    if current["mssql-tds"] != current["mssql-mock-tds"]:
        raise ValueError(
            f"mssql-tds {current['mssql-tds']} and mssql-mock-tds {current['mssql-mock-tds']} "
            "must match for a release; choose versions manually."
        )
    selected = [crate for crate in CRATES if current[crate] in published[crate]]
    if "mssql-mock-tds" in selected and "mssql-tds" not in selected:
        selected.remove("mssql-mock-tds")
        print("mssql-mock-tds: waiting for its mssql-tds dependency to be published.")
    changes = {crate: (current[crate], next_minor(current[crate])) for crate in selected}
    if "mssql-tds" in changes:
        core_target = changes["mssql-tds"][1]
        mock_version = current["mssql-mock-tds"]
        if version_tuple(mock_version) < version_tuple(core_target):
            changes["mssql-mock-tds"] = (mock_version, core_target)
        elif version_tuple(mock_version) > version_tuple(core_target):
            raise ValueError(
                f"mssql-mock-tds {mock_version} is ahead of mssql-tds target {core_target}; "
                "choose versions manually."
            )
        elif mock_version in published["mssql-mock-tds"]:
            raise ValueError(
                f"mssql-mock-tds {mock_version} is already published; choose the next version manually."
            )
        else:
            changes.pop("mssql-mock-tds", None)
    for crate, (_old, new) in changes.items():
        if new in published[crate]:
            raise ValueError(f"{crate} {new} is already published; choose the next version manually.")
    return changes


def has_open_bump_pr(root, changes):
    result = subprocess.run(
        [
            "gh", "pr", "list", "--repo", os.environ["GITHUB_REPOSITORY"],
            "--base", os.environ["DEFAULT_BRANCH"], "--state", "open",
            "--json", "number,title,body", "--limit", "100",
        ],
        cwd=root, check=True, stdout=subprocess.PIPE, text=True,
    )
    return any(
        all(
            summary_line(crate, old, new) in text
            for crate, (old, new) in changes.items()
        )
        for pr in json.loads(result.stdout)
        for text in [f"{pr.get('title') or ''}\n{pr.get('body') or ''}"]
    )


def issue_body(summary, changes):
    snippets = []
    for crate, (_old, new) in changes.items():
        snippets.append(f"{crate}/Cargo.toml\n```toml\nversion = \"{new}\"\n```")
    if "mssql-tds" in changes:
        snippets.append(
            "mssql-mock-tds/Cargo.toml dependency\n"
            "```toml\n"
            f"mssql-tds = {{ path = \"../mssql-tds\", version = \"{changes['mssql-tds'][1]}\", default-features = false }}\n"
            "```"
        )
    return (
        f"{ISSUE_MARKER}\n\n"
        "### Problem statement\n\n"
        "The default branch uses crate versions that are already published on crates.io.\n\n"
        "### Proposed solution\n\n"
        f"Update these versions:\n\n{summary}\n\n"
        "Suggested edits:\n\n"
        + "\n\n".join(snippets)
        + "\n\nInstructions:\n\n"
        "1. Apply the version changes above.\n"
        "2. Run `cargo fetch` (or `cargo update --workspace --offline`) to refresh `Cargo.lock` after the version bump.\n"
        "3. Run `cargo bfmt`, `cargo bclippy`, and `cargo btest`.\n"
        "4. Open a PR and include `Fixes #<this issue number>` plus the version "
        "summary above in the description.\n\n"
        "### Affected crate\n\n"
        + ", ".join(f"`{crate}`" for crate in changes)
        + "\n\n"
        "### Alternatives considered\n\nBump the versions manually.\n\n"
        "### Additional context\n\n"
        "Managed by the Bump Released Crate Versions workflow. "
        "This workflow regenerates the issue body while the bump remains pending; "
        "put maintainer notes in issue comments. "
        "This workflow creates or updates this issue only; maintainers own the PR and validation.\n"
    )


def ensure_bump_issue(summary, changes):
    endpoint = f"repos/{os.environ['GITHUB_REPOSITORY']}/issues"
    owner, name = os.environ["GITHUB_REPOSITORY"].split("/")
    query = """
    query($owner: String!, $name: String!, $label: String!, $endCursor: String) {
      repository(owner: $owner, name: $name) {
        issues(first: 100, after: $endCursor, states: OPEN, labels: [$label]) {
          nodes { number body }
          pageInfo { hasNextPage endCursor }
        }
      }
    }
    """
    result = subprocess.run(
        ["gh", "api", "graphql", "-f", f"query={query}", "-f", f"owner={owner}",
         "-f", f"name={name}", "-f", f"label={ISSUE_LABEL}", "--paginate", "--slurp"],
        check=True, stdout=subprocess.PIPE, text=True,
    )
    matches = [
        issue for page in json.loads(result.stdout)
        for issue in page["data"]["repository"]["issues"]["nodes"]
        if (issue.get("body") or "").startswith(ISSUE_MARKER)
    ]
    if len(matches) > 1:
        raise ValueError("Multiple open version bump tracking issues; resolve duplicates manually.")

    body = issue_body(summary, changes)
    if matches and matches[0]["body"] == body:
        number = matches[0]["number"]
        print(f"Reusing version bump issue #{number}.")
        return number

    payload = {"body": body}
    if matches:
        endpoint += f"/{matches[0]['number']}"
        method = "PATCH"
    else:
        subprocess.run(
            ["gh", "label", "create", ISSUE_LABEL, "--repo", os.environ["GITHUB_REPOSITORY"],
             "--color", "0e8a16", "--description", "Tracking issues for automated Rust crate version bumps",
             "--force"],
            check=True, stdout=subprocess.PIPE, text=True,
        )
        payload["title"] = "Bump released crates to the next minor version"
        payload["labels"] = [ISSUE_LABEL]
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
    versions = {crate: published_versions(crate) for crate in CRATES}
    changes = planned_bumps(root, versions)
    if not changes:
        print("No version bumps needed.")
        return
    if has_open_bump_pr(root, changes):
        print("Open version bump PR already exists; skipping issue creation.")
        return
    summary = "\n".join(
        summary_line(crate, current, bumped)
        for crate, (current, bumped) in changes.items()
    )
    ensure_bump_issue(summary, changes)
    print(summary)


if __name__ == "__main__":
    main()

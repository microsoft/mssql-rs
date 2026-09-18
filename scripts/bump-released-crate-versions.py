# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Prepare a draft PR when either source crate version is already on crates.io."""

import json
import os
import subprocess
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

CRATES = ("mssql-tds", "mssql-mock-tds")
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


def ensure_bump_issue(summary, crates):
    endpoint = f"repos/{os.environ['GITHUB_REPOSITORY']}/issues"
    # List directly rather than searching: search indexing can lag a previous run.
    result = subprocess.run(
        ["gh", "api", f"{endpoint}?state=open&per_page=100", "--paginate", "--slurp"],
        check=True, stdout=subprocess.PIPE, text=True,
    )
    matches = [
        issue for page in json.loads(result.stdout) for issue in page
        if "pull_request" not in issue
        and (issue.get("body") or "").startswith(ISSUE_MARKER)
    ]
    if len(matches) > 1:
        raise ValueError("Multiple open version bump tracking issues; resolve duplicates manually.")

    body = (
        f"{ISSUE_MARKER}\n\n"
        "### Problem statement\n\n"
        "The default branch uses crate versions that are already published on crates.io.\n\n"
        "### Proposed solution\n\n"
        f"Start the next minor development versions:\n\n{summary}\n\n"
        "### Affected crate\n\n"
        + ", ".join(f"`{crate}`" for crate in crates)
        + "\n\n### Alternatives considered\n\nBump the versions manually.\n\n"
        "### Additional context\n\n"
        "Managed by the Bump Released Crate Versions workflow. "
        "The draft PR links this issue and keeps local versioned dependencies in sync. "
        "No crates are published by this workflow.\n"
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
    # Fetch both before editing: a registry outage must not produce a partial PR.
    versions = {crate: published_versions(crate) for crate in CRATES}
    changes = bump_versions(root, versions)
    body = (root / ".github" / "PULL_REQUEST_TEMPLATE.md").read_text(encoding="utf-8")
    if changes:
        summary = "\n".join(
            f"- `{crate}`: `{current}` -> `{bumped}`"
            for crate, (current, bumped) in changes.items()
        )
        issue = ensure_bump_issue(summary, changes)
        body = body.replace(
            "## Description\n",
            "## Description\n\nThese source versions are already published on crates.io. "
            "Start the next minor development version:\n\n"
            f"{summary}\n\nLocal versioned dependencies are kept in sync. "
            "This PR does not publish crates. Validation and review are still required.\n",
        ).replace("## Related Issues\n", f"## Related Issues\n\nFixes #{issue}\n")
        print(summary)
    else:
        print("No version bumps needed.")

    # Run the PR action even without changes so it can close an obsolete bump PR.
    (Path(os.environ["RUNNER_TEMP"]) / "crate-version-bump-pr.md").write_text(
        body, encoding="utf-8"
    )


if __name__ == "__main__":
    main()

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Prepare a draft PR when either source crate version is already on crates.io."""

import json
import os
import re
import subprocess
import tomllib
from datetime import date, datetime, timezone
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

CRATES = ("mssql-tds", "mssql-mock-tds")
ISSUE_MARKER = "<!-- mssql-rs:released-crate-version-bump -->"


def is_due(today):
    return (today - date(1970, 1, 1)).days % 3 == 0


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


def next_minor(version):
    match = re.fullmatch(
        r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?"
        r"(?:\+[0-9A-Za-z.-]+)?",
        version,
    )
    if not match:
        raise ValueError(f"Unsupported crate version: {version}")
    return f"{match[1]}.{int(match[2]) + 1}.0"


def replace_once(pattern, replacement, text):
    updated, count = re.subn(pattern, replacement, text, flags=re.MULTILINE)
    if count != 1:
        raise ValueError(f"Expected one manifest match for {pattern!r}, found {count}")
    return updated


def prepare_bump(root, versions):
    contents = {
        crate: (root / crate / "Cargo.toml").read_text(encoding="utf-8")
        for crate in CRATES
    }
    changes = {}
    for crate in CRATES:
        current = tomllib.loads(contents[crate])["package"]["version"]
        if current not in versions[crate]:
            print(f"{crate} {current}: not published; leaving its version unchanged.")
            continue
        bumped = next_minor(current)
        if bumped in versions[crate]:
            raise ValueError(f"{crate} {bumped} is already published; bump it manually.")
        changes[crate] = (current, bumped)
        contents[crate] = replace_once(
            r'(^\[package\][\s\S]*?^version\s*=\s*)"' + re.escape(current) + r'"',
            lambda match: f'{match[1]}"{bumped}"',
            contents[crate],
        )

    if "mssql-tds" in changes:
        bumped = changes["mssql-tds"][1]
        contents["mssql-mock-tds"] = replace_once(
            r'(^mssql-tds\s*=\s*\{[^\r\n}]*?\bversion\s*=\s*)"[^"]+"',
            lambda match: f'{match[1]}"{bumped}"',
            contents["mssql-mock-tds"],
        )
        dependency = tomllib.loads(contents["mssql-mock-tds"])["dependencies"]["mssql-tds"]
        if dependency["version"] != bumped or dependency["path"] != "../mssql-tds":
            raise ValueError("The mock crate must depend on the updated local mssql-tds")

    for crate, (_, bumped) in changes.items():
        if tomllib.loads(contents[crate])["package"]["version"] != bumped:
            raise ValueError(f"Failed to update {crate}'s package version")
    return contents, changes


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
    if os.environ.get("GITHUB_EVENT_NAME") == "schedule" and not is_due(
        datetime.now(timezone.utc).date()
    ):
        print("Not a three-day check date; skipping.")
        return

    root = Path(__file__).resolve().parents[1]
    # Fetch both before editing: a registry outage must not produce a partial PR.
    versions = {crate: published_versions(crate) for crate in CRATES}
    contents, changes = prepare_bump(root, versions)
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
        for crate, content in contents.items():
            path = root / crate / "Cargo.toml"
            if path.read_text(encoding="utf-8") != content:
                path.write_text(content, encoding="utf-8", newline="\n")
        print(summary)
    else:
        print("No version bumps needed.")

    # Run the PR action even without changes so it can close an obsolete bump PR.
    (Path(os.environ["RUNNER_TEMP"]) / "crate-version-bump-pr.md").write_text(
        body, encoding="utf-8"
    )
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write("due=true\n")


if __name__ == "__main__":
    main()

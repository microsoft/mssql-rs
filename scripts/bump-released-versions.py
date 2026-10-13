# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Open version bump pull requests when released package versions are already published.

Two independent tracks run per invocation:

- the released Rust crates (``mssql-tds``, ``mssql-mock-tds``) against crates.io;
- the ``mssql-python-rs`` distribution in ``mssql-py-core/pyproject.toml`` against PyPI.

Each track files a tracking issue, pushes a branch, and opens one pull request
bumping to the next minor version. A failure in one track does not stop the other.
Maintainers own review, validation, and the final release version: push to the
open PR to choose a different version.
"""

import json
import os
import re
import subprocess
import time
import traceback
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

CRATES = ("mssql-tds", "mssql-mock-tds")
PYTHON_DIST = "mssql-python-rs"
PYPROJECT = Path("mssql-py-core") / "pyproject.toml"

CRATE_LABEL = "automation:crate-version-bump"
CRATE_MARKER = "<!-- mssql-rs:released-crate-version-bump -->"
CRATE_BRANCH = "automation/bump-released-crates"

PYTHON_LABEL = "automation:python-version-bump"
PYTHON_MARKER = "<!-- mssql-rs:released-python-version-bump -->"
PYTHON_BRANCH = "automation/bump-mssql-python-rs"

USER_AGENT = "microsoft/mssql-rs version check (https://github.com/microsoft/mssql-rs)"
RETRYABLE_STATUS = {429, 500, 502, 503, 504}

# A `version` assignment inside a named TOML table, matching the first such key
# before the next table header so `[package].version` is never confused with a
# version requirement in `[dependencies]`.
def table_version_pattern(table):
    return re.compile(
        rf"^(\[{re.escape(table)}\]\n(?:(?!\[).*\n)*?version = )\"([^\"]+)\"$",
        re.MULTILINE,
    )


MOCK_TDS_DEPENDENCY = re.compile(
    r"^(mssql-tds = \{ path = \"\.\./mssql-tds\", version = )\"[^\"]+\"",
    re.MULTILINE,
)


def fetch_registry_json(url, package):
    """Return the decoded registry payload, or None when the package is unpublished."""
    for attempt in range(3):
        request = Request(url, headers={"User-Agent": USER_AGENT})
        try:
            with urlopen(request, timeout=30) as response:
                return json.load(response)
        except HTTPError as error:
            if error.code == 404:
                print(f"{package}: not published; leaving its version unchanged.")
                return None
            if error.code in RETRYABLE_STATUS and attempt < 2:
                time.sleep(2 ** attempt)
                continue
            raise
        except URLError:
            if attempt < 2:
                time.sleep(2 ** attempt)
                continue
            raise


def published_versions(crate):
    data = fetch_registry_json(f"https://crates.io/api/v1/crates/{crate}", crate)
    if data is None:
        return set()
    versions = {entry["num"] for entry in data["versions"]}
    if not versions or not all(isinstance(version, str) for version in versions):
        raise ValueError(f"{crate}: invalid crates.io versions response")
    return versions


def pypi_published_versions(distribution):
    data = fetch_registry_json(f"https://pypi.org/pypi/{distribution}/json", distribution)
    if data is None:
        return set()
    versions = set(data["releases"])
    if not versions or not all(isinstance(version, str) for version in versions):
        raise ValueError(f"{distribution}: invalid PyPI releases response")
    return versions


def read_table_version(path, table):
    match = table_version_pattern(table).search(path.read_text(encoding="utf-8"))
    if match is None:
        raise ValueError(f"{path}: no [{table}] version field found.")
    return match.group(2)


def write_table_version(path, table, version):
    text = path.read_text(encoding="utf-8")
    updated, count = table_version_pattern(table).subn(
        lambda match: f'{match.group(1)}"{version}"', text, count=1
    )
    if count != 1:
        raise ValueError(f"{path}: no [{table}] version field to update.")
    path.write_text(updated, encoding="utf-8")


def write_mock_tds_dependency(path, version):
    text = path.read_text(encoding="utf-8")
    updated, count = MOCK_TDS_DEPENDENCY.subn(
        lambda match: f'{match.group(1)}"{version}"', text, count=1
    )
    if count != 1:
        raise ValueError(f"{path}: no versioned mssql-tds path dependency to update.")
    path.write_text(updated, encoding="utf-8")


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


def summary_line(package, old, new):
    return f"- `{package}`: `{old}` -> `{new}`"


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


def planned_python_bump(root, published):
    current = read_table_version(root / PYPROJECT, "project")
    if current not in published:
        return {}
    target = next_minor(current)
    if target in published:
        raise ValueError(
            f"{PYTHON_DIST} {target} is already published; choose the next version manually."
        )
    return {PYTHON_DIST: (current, target)}


def apply_crate_bumps(root, changes):
    for crate, (_old, new) in changes.items():
        write_table_version(root / crate / "Cargo.toml", "package", new)
    if "mssql-tds" in changes:
        write_mock_tds_dependency(
            root / "mssql-mock-tds" / "Cargo.toml", changes["mssql-tds"][1]
        )
    return sorted({f"{crate}/Cargo.toml" for crate in (*changes, "mssql-mock-tds")})


def apply_python_bump(root, changes):
    write_table_version(root / PYPROJECT, "project", changes[PYTHON_DIST][1])
    return [PYPROJECT.as_posix()]


def has_open_bump_pr(root, changes, branch):
    """True when a pull request other than ours already proposes the same transitions."""
    result = subprocess.run(
        [
            "gh", "pr", "list", "--repo", os.environ["GITHUB_REPOSITORY"],
            "--base", os.environ["DEFAULT_BRANCH"], "--state", "open",
            "--json", "number,title,body,headRefName", "--limit", "100",
        ],
        cwd=root, check=True, stdout=subprocess.PIPE, text=True,
    )
    return any(
        all(
            summary_line(package, old, new) in text
            for package, (old, new) in changes.items()
        )
        for pr in json.loads(result.stdout)
        if pr.get("headRefName") != branch
        for text in [f"{pr.get('title') or ''}\n{pr.get('body') or ''}"]
    )


def issue_body(marker, summary, affected, snippets):
    return (
        f"{marker}\n\n"
        "### Problem statement\n\n"
        "The default branch uses versions that are already published, so the next "
        "release has no version left to claim.\n\n"
        "### Proposed solution\n\n"
        f"Update these versions:\n\n{summary}\n\n"
        + "\n\n".join(snippets)
        + "\n\n### Affected crate\n\n"
        + affected
        + "\n\n### Alternatives considered\n\nBump the versions manually.\n\n"
        "### Additional context\n\n"
        "Managed by the Bump Released Versions workflow, which opens the linked pull "
        "request and regenerates this body while the bump is pending; put maintainer "
        "notes in issue comments. Push to the pull request to choose a different "
        "release version.\n"
    )


def crate_issue_body(summary, changes):
    snippets = [
        f"{crate}/Cargo.toml\n```toml\nversion = \"{new}\"\n```"
        for crate, (_old, new) in changes.items()
    ]
    if "mssql-tds" in changes:
        snippets.append(
            "mssql-mock-tds/Cargo.toml dependency\n"
            "```toml\n"
            f"mssql-tds = {{ path = \"../mssql-tds\", version = \"{changes['mssql-tds'][1]}\", default-features = false }}\n"
            "```"
        )
    affected = ", ".join(f"`{crate}`" for crate in changes)
    return issue_body(CRATE_MARKER, summary, affected, snippets)


def python_issue_body(summary, changes):
    snippet = (
        f"{PYPROJECT.as_posix()}\n```toml\n"
        f"version = \"{changes[PYTHON_DIST][1]}\"\n```"
    )
    return issue_body(
        PYTHON_MARKER,
        summary,
        "`mssql-py-core`",
        [
            snippet,
            "The independently versioned `mssql-py-core` Rust crate is unchanged.",
        ],
    )


def ensure_bump_issue(label, marker, title, body):
    repository = os.environ["GITHUB_REPOSITORY"]
    endpoint = f"repos/{repository}/issues"
    owner, name = repository.split("/")
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
         "-f", f"name={name}", "-f", f"label={label}", "--paginate", "--slurp"],
        check=True, stdout=subprocess.PIPE, text=True,
    )
    matches = [
        issue for page in json.loads(result.stdout)
        for issue in page["data"]["repository"]["issues"]["nodes"]
        if (issue.get("body") or "").startswith(marker)
    ]
    if len(matches) > 1:
        raise ValueError(f"Multiple open {label} tracking issues; resolve duplicates manually.")

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
            ["gh", "label", "create", label, "--repo", repository,
             "--color", "0e8a16", "--description", "Tracking issues for automated version bumps",
             "--force"],
            check=True, stdout=subprocess.PIPE, text=True,
        )
        payload["title"] = title
        payload["labels"] = [label]
        method = "POST"
    result = subprocess.run(
        ["gh", "api", endpoint, "--method", method, "--input", "-"],
        input=json.dumps(payload), check=True, stdout=subprocess.PIPE, text=True,
    )
    number = json.loads(result.stdout)["number"]
    print(f"{'Updated' if matches else 'Created'} version bump issue #{number}.")
    return number


def pull_request_body(summary, issue_number, extra):
    return (
        "## Description\n\n"
        "The default branch uses versions that are already published. Advance them to "
        f"the next minor version:\n\n{summary}\n\n{extra}\n\n"
        "## Related Issues\n\n"
        f"Fixes #{issue_number}\n\n"
        "## Checklist\n\n"
        "- [ ] `cargo bfmt` passes\n"
        "- [ ] `cargo bclippy` passes\n"
        "- [ ] `cargo btest` passes\n"
        "- [x] New/changed functionality has tests\n"
        "- [x] Public API changes are documented\n\n"
        "Opened by the Bump Released Versions workflow. Push to this branch to "
        "release a different version; the workflow only force-updates the branch "
        "while the planned versions change.\n"
    )


def git(root, *args):
    return subprocess.run(
        ["git", *args], cwd=root, check=True, stdout=subprocess.PIPE, text=True
    ).stdout


def open_bump_pr(root, branch, base_sha, title, body, apply_changes):
    repository = os.environ["GITHUB_REPOSITORY"]
    base = os.environ["DEFAULT_BRANCH"]
    # Branch first so each track commits only its own edits, even when an earlier
    # track already left a commit on a different branch.
    git(root, "switch", "--force-create", branch, base_sha)
    paths = apply_changes(root)
    git(root, "add", "--", *paths)
    git(root, "commit", "--message", title)
    git(root, "push", "--force", "origin", f"{branch}:refs/heads/{branch}")
    existing = json.loads(subprocess.run(
        ["gh", "pr", "list", "--repo", repository, "--head", branch,
         "--base", base, "--state", "open", "--json", "number", "--limit", "1"],
        cwd=root, check=True, stdout=subprocess.PIPE, text=True,
    ).stdout)
    if existing:
        number = existing[0]["number"]
        subprocess.run(
            ["gh", "pr", "edit", str(number), "--repo", repository,
             "--title", title, "--body", body],
            cwd=root, check=True, stdout=subprocess.PIPE, text=True,
        )
        print(f"Updated bump PR #{number}.")
        return number
    url = subprocess.run(
        ["gh", "pr", "create", "--repo", repository, "--base", base, "--head", branch,
         "--title", title, "--body", body],
        cwd=root, check=True, stdout=subprocess.PIPE, text=True,
    ).stdout.strip()
    print(f"Created bump PR {url}")
    return url


def run_crate_track(root, base_sha):
    published = {crate: published_versions(crate) for crate in CRATES}
    changes = planned_bumps(root, published)
    if not changes:
        print("No crate version bumps needed.")
        return
    if has_open_bump_pr(root, changes, CRATE_BRANCH):
        print("Another open crate bump PR already proposes these versions; skipping.")
        return
    summary = "\n".join(
        summary_line(crate, old, new) for crate, (old, new) in changes.items()
    )
    number = ensure_bump_issue(
        CRATE_LABEL, CRATE_MARKER,
        "Bump released crates to the next minor version",
        crate_issue_body(summary, changes),
    )
    target = changes["mssql-tds"][1] if "mssql-tds" in changes else next(iter(changes.values()))[1]
    open_bump_pr(
        root, CRATE_BRANCH, base_sha,
        f"Bump released TDS crates to {target}",
        pull_request_body(
            summary, number,
            "The mock crate's versioned `mssql-tds` requirement is updated with it, "
            "preserving its path and disabled default features.",
        ),
        lambda branch_root: apply_crate_bumps(branch_root, changes),
    )
    print(summary)


def run_python_track(root, base_sha):
    published = pypi_published_versions(PYTHON_DIST)
    changes = planned_python_bump(root, published)
    if not changes:
        print(f"No {PYTHON_DIST} version bump needed.")
        return
    if has_open_bump_pr(root, changes, PYTHON_BRANCH):
        print(f"Another open {PYTHON_DIST} bump PR already proposes this version; skipping.")
        return
    old, new = changes[PYTHON_DIST]
    summary = summary_line(PYTHON_DIST, old, new)
    number = ensure_bump_issue(
        PYTHON_LABEL, PYTHON_MARKER,
        f"Bump {PYTHON_DIST} to the next minor version",
        python_issue_body(summary, changes),
    )
    open_bump_pr(
        root, PYTHON_BRANCH, base_sha,
        f"Bump {PYTHON_DIST} to {new}",
        pull_request_body(
            summary, number,
            "The independently versioned `mssql-py-core` Rust crate is unchanged.",
        ),
        lambda branch_root: apply_python_bump(branch_root, changes),
    )
    print(summary)


def main():
    root = Path(__file__).resolve().parents[1]
    base_sha = git(root, "rev-parse", "HEAD").strip()
    failures = []
    for name, track in (("crates", run_crate_track), ("python", run_python_track)):
        try:
            track(root, base_sha)
        except Exception:
            # Keep the tracks independent: a crates.io outage must not block the
            # PyPI-gated bump, and vice versa.
            traceback.print_exc()
            failures.append(name)
    if failures:
        raise SystemExit(f"Version bump tracks failed: {', '.join(failures)}")


if __name__ == "__main__":
    main()

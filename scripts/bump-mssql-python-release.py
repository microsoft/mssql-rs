# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Open a draft mssql-python PR for a published mssql-python-rs release."""

import base64
import json
import os
import subprocess
import tempfile
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

API_ROOT = "https://api.github.com"
PYPI_NAME = "mssql-python-rs"
NUGET_NAME = "mssql-python-rs-wheels"
NUGET_INDEX = (
    "https://pkgs.dev.azure.com/SqlClientDrivers/"
    "904996cc-6198-4d39-8540-eca72bdf0b7b/_packaging/"
    "376106a7-58f4-4fec-82bd-05b84bbea288/nuget/v3/flat2/"
    f"{NUGET_NAME}/index.json"
)
PYPI_PIN = Path("eng/versions/mssql-python-rs.version")
NUGET_PIN = Path("eng/versions/mssql-python-rs-nuget.version")
PR_TEMPLATE = Path(".github/PULL_REQUEST_TEMPLATE.MD")
BRANCH = "automation/update-mssql-python-rs"
ISSUE_LABEL = "automation:dependency-version-bump"
ISSUE_MARKER = "<!-- mssql-rs:mssql-python-release-bump -->"
USER_AGENT = "microsoft/mssql-rs release automation"
RETRYABLE_STATUS = {429, 500, 502, 503, 504}


def version_tuple(version):
    parts = version.split(".")
    if len(parts) != 3 or any(not part.isdigit() for part in parts):
        raise ValueError(f"{version!r} is not a three-part release version")
    return tuple(int(part) for part in parts)


def fetch_json(url, missing_is_none=False):
    for attempt in range(3):
        try:
            with urlopen(Request(url, headers={"User-Agent": USER_AGENT}), timeout=30) as response:
                return json.load(response)
        except HTTPError as error:
            if missing_is_none and error.code == 404:
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


def release_state(version):
    pypi = fetch_json(f"https://pypi.org/pypi/{PYPI_NAME}/{version}/json", True)
    if pypi is None:
        raise ValueError(f"{PYPI_NAME} {version} is not available on PyPI")
    if pypi.get("info", {}).get("version") != version:
        raise ValueError(f"PyPI returned unexpected metadata for {PYPI_NAME} {version}")

    nuget = fetch_json(NUGET_INDEX)
    versions = nuget.get("versions")
    if not isinstance(versions, list) or not all(isinstance(item, str) for item in versions):
        raise ValueError(f"{NUGET_NAME}: invalid NuGet version index")
    return version in versions


def github_api(method, path, payload=None, allow_status=()):
    body = json.dumps(payload).encode() if payload is not None else None
    authorization = "Bearer " + os.environ["GH_TOKEN"]
    request = Request(
        f"{API_ROOT}{path}",
        data=body,
        method=method,
        headers={
            "Authorization": authorization,
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "Content-Type": "application/json",
            "User-Agent": USER_AGENT,
        },
    )
    # POST is not retried: the server may have completed a create operation
    # before a failed response reached us. The next pipeline run reconciles by
    # marker or branch instead of risking duplicate issues or pull requests.
    attempts = 1 if method == "POST" else 3
    for attempt in range(attempts):
        try:
            with urlopen(request, timeout=30) as response:
                raw = response.read()
                return response.status, (json.loads(raw) if raw else None)
        except HTTPError as error:
            if error.code in allow_status:
                return error.code, None
            if error.code in RETRYABLE_STATUS and attempt + 1 < attempts:
                time.sleep(2 ** attempt)
                continue
            detail = error.read().decode(errors="replace")[:500]
            raise RuntimeError(f"{method} {path} failed: HTTP {error.code} {detail}") from error
        except URLError:
            if attempt + 1 < attempts:
                time.sleep(2 ** attempt)
                continue
            raise


def github_paged(path):
    separator = "&" if "?" in path else "?"
    items = []
    for page in range(1, 11):
        _status, batch = github_api("GET", f"{path}{separator}per_page=100&page={page}")
        if not batch:
            break
        items.extend(batch)
        if len(batch) < 100:
            break
    return items


def repository():
    return os.environ.get("MSSQL_PYTHON_REPOSITORY", "microsoft/mssql-python")


def git_auth_environment():
    env = os.environ.copy()
    credential = base64.b64encode(f"x-access-token:{os.environ['GH_TOKEN']}".encode()).decode()
    env.update(
        {
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "http.https://github.com/.extraheader",
            "GIT_CONFIG_VALUE_0": f"AUTHORIZATION: basic {credential}",
        }
    )
    return env


def git(root, *args, check=True):
    return subprocess.run(
        ["git", *args],
        cwd=root,
        check=check,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=git_auth_environment(),
    )


def read_pin(root, path):
    value = (root / path).read_text(encoding="utf-8").strip()
    version_tuple(value)
    return value


def write_pin(root, path, version):
    target = root / path
    original = target.read_text(encoding="utf-8")
    suffix = "\n" if original.endswith("\n") else ""
    target.write_text(version + suffix, encoding="utf-8")


def planned_changes(root, released_version, nuget_available):
    released = version_tuple(released_version)
    python_current = read_pin(root, PYPI_PIN)
    nuget_current = read_pin(root, NUGET_PIN)
    changes = {}

    if version_tuple(python_current) < released:
        changes[PYPI_PIN] = (python_current, released_version)
    elif version_tuple(python_current) > released:
        print(
            f"Ignoring stale release {released_version}: {PYPI_PIN} is already "
            f"{python_current}."
        )
        return {}

    if nuget_available and version_tuple(nuget_current) < released:
        changes[NUGET_PIN] = (nuget_current, released_version)
    elif version_tuple(nuget_current) > released:
        print(f"Leaving newer {NUGET_PIN} {nuget_current} unchanged.")
    elif not nuget_available:
        print(
            f"{NUGET_NAME} {released_version} is not available; leaving "
            f"{NUGET_PIN} at {nuget_current}."
        )
    return changes


def issue_body(version, changes, source_build_id):
    lines = "\n".join(
        f"- `{path.as_posix()}`: `{old}` -> `{new}`"
        for path, (old, new) in changes.items()
    )
    source = (
        f"[Azure Pipelines build {source_build_id}]"
        f"(https://sqlclientdrivers.visualstudio.com/mssql-rs/_build/results"
        f"?buildId={source_build_id})"
        if source_build_id
        else "a manual worker run"
    )
    return (
        f"{ISSUE_MARKER}\n\n"
        f"`{PYPI_NAME}` {version} was published by {source}. Update the "
        "`mssql-python` dependency pins that have matching released artifacts.\n\n"
        f"{lines}\n\n"
        f"`{NUGET_PIN.as_posix()}` is changed only when the exact clean "
        f"`{NUGET_NAME}` version is present in Azure Artifacts.\n"
    )


def ensure_issue(version, changes, source_build_id):
    repo = repository()
    candidates = github_paged(f"/repos/{repo}/issues?state=open&labels={ISSUE_LABEL}")
    matches = [
        issue
        for issue in candidates
        if "pull_request" not in issue and (issue.get("body") or "").startswith(ISSUE_MARKER)
    ]
    if len(matches) > 1:
        raise ValueError(f"Multiple open {ISSUE_LABEL} issues; resolve them manually")
    body = issue_body(version, changes, source_build_id)
    if matches:
        number = matches[0]["number"]
        if matches[0].get("body") != body:
            github_api("PATCH", f"/repos/{repo}/issues/{number}", {"body": body})
        return number

    github_api(
        "POST",
        f"/repos/{repo}/labels",
        {
            "name": ISSUE_LABEL,
            "color": "0e8a16",
            "description": "Automated mssql-python-rs dependency updates",
        },
        allow_status=(422,),
    )
    _status, created = github_api(
        "POST",
        f"/repos/{repo}/issues",
        {
            "title": f"Update {PYPI_NAME} dependency to {version}",
            "body": body,
            "labels": [ISSUE_LABEL],
        },
    )
    return created["number"]


def pull_request_body(root, issue_number, version, changes):
    summary = "\n".join(
        f"- `{path.as_posix()}`: `{old}` -> `{new}`"
        for path, (old, new) in changes.items()
    )
    template = (root / PR_TEMPLATE).read_text(encoding="utf-8")
    template = template.replace("> AB#<WORK_ITEM_ID>", "> AB#N/A")
    template = template.replace(
        "> GitHub Issue: #<ISSUE_NUMBER>", f"> GitHub Issue: #{issue_number}"
    )
    placeholder = "<!-- Insert your summary of changes below. Minimum 10 characters required. -->"
    if placeholder not in template:
        raise ValueError(f"{PR_TEMPLATE}: summary placeholder not found")
    detail = (
        f"Update released `{PYPI_NAME}` dependency pins to version `{version}`.\n\n"
        f"{summary}\n\n"
        f"The NuGet pin changes only when `{NUGET_NAME} {version}` is already "
        "available. This draft does not publish or merge anything."
    )
    return template.replace(placeholder, f"{placeholder}\n\n{detail}", 1)


def remote_branch_state(root, base_sha):
    fetched = git(root, "fetch", "--quiet", "origin", BRANCH, check=False)
    if fetched.returncode != 0:
        return False, None, True
    tree = git(root, "rev-parse", "FETCH_HEAD^{tree}").stdout.strip()
    commits = git(root, "rev-list", f"{base_sha}..FETCH_HEAD").stdout.split()
    committer = git(root, "log", "-1", "--format=%cn", "FETCH_HEAD").stdout.strip()
    owned = len(commits) <= 1 and committer == os.environ.get(
        "GIT_COMMITTER_NAME", "Azure Pipelines"
    )
    return True, tree, owned


def update_pull_request(root, version, changes, source_build_id):
    repo = repository()
    base = os.environ.get("DEFAULT_BRANCH", "main")
    owner = repo.split("/", 1)[0]
    git(root, "fetch", "--quiet", "origin", base)
    base_sha = git(root, "rev-parse", "FETCH_HEAD").stdout.strip()
    exists, remote_tree, owned = remote_branch_state(root, base_sha)
    existing = github_paged(
        f"/repos/{repo}/pulls?state=open&base={base}&head={owner}:{BRANCH}"
    )
    if exists and not owned:
        number = existing[0]["number"] if existing else None
        print(
            f"{BRANCH} carries maintainer commits; leaving it"
            + (f" and PR #{number}" if number else "")
            + " untouched."
        )
        return number

    issue_number = ensure_issue(version, changes, source_build_id)
    title = f"CHORE: Update {PYPI_NAME} dependency to {version}"
    body = pull_request_body(root, issue_number, version, changes)
    git(root, "switch", "--discard-changes", "--force-create", BRANCH, base_sha)
    for path, (_old, new) in changes.items():
        write_pin(root, path, new)
    paths = [path.as_posix() for path in changes]
    git(root, "add", "--", *paths)
    git(root, "commit", "--message", f"Update {PYPI_NAME} dependency to {version}")
    local_tree = git(root, "rev-parse", "HEAD^{tree}").stdout.strip()
    if local_tree == remote_tree:
        print(f"{BRANCH} already contains these dependency versions.")
    else:
        git(root, "push", "--force", "origin", f"{BRANCH}:refs/heads/{BRANCH}")

    if existing:
        number = existing[0]["number"]
        github_api("PATCH", f"/repos/{repo}/pulls/{number}", {"title": title, "body": body})
        print(f"Updated microsoft/mssql-python PR #{number}.")
        return number
    _status, created = github_api(
        "POST",
        f"/repos/{repo}/pulls",
        {"title": title, "head": BRANCH, "base": base, "body": body, "draft": True},
    )
    print(f"Created draft microsoft/mssql-python PR #{created['number']}.")
    return created["number"]


def main():
    version = os.environ.get("RELEASED_VERSION", "").strip()
    version_tuple(version)
    nuget_available = release_state(version)
    source_build_id = os.environ.get("SOURCE_BUILD_ID", "").strip()

    with tempfile.TemporaryDirectory(prefix="mssql-python-release-") as directory:
        root = Path(directory) / "mssql-python"
        git(
            Path(directory),
            "clone",
            "--quiet",
            "--no-tags",
            f"https://github.com/{repository()}.git",
            str(root),
        )
        changes = planned_changes(root, version, nuget_available)
        if not changes:
            print(f"No mssql-python dependency changes are needed for {version}.")
            return
        update_pull_request(root, version, changes, source_build_id)


if __name__ == "__main__":
    main()

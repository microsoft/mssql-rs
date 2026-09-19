# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import importlib.util
import io
import json
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path
from urllib.error import HTTPError, URLError
from unittest.mock import Mock, patch

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "bump", ROOT / "scripts" / "bump-released-crate-versions.py"
)
bump = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bump)


@pytest.mark.parametrize("released", [(), ("mssql-tds",), bump.CRATES])
def test_selects_only_released_crates_and_repeat_is_quiet(tmp_path, released):
    before = dict.fromkeys(bump.CRATES, "0.1.7")
    after = {crate: "0.2.0" if crate in released else before[crate] for crate in bump.CRATES}
    published = {crate: {"0.1.7"} if crate in released else set() for crate in bump.CRATES}
    with patch.object(bump, "cargo_versions", side_effect=[before, after, after]), patch.object(
        bump.subprocess, "run"
    ) as cargo:
        assert bump.bump_versions(tmp_path, published) == {
            crate: ("0.1.7", "0.2.0") for crate in released
        }
        assert bump.bump_versions(tmp_path, published) == {}
    if released:
        cargo.assert_called_once_with(
            ["cargo", "set-version", "--bump", "minor"]
            + [arg for crate in released for arg in ("--package", crate)],
            cwd=tmp_path, check=True,
        )
    else:
        cargo.assert_not_called()


def test_mock_bump_waits_for_published_core(tmp_path, capsys):
    before = dict.fromkeys(bump.CRATES, "0.1.7")
    published = {"mssql-tds": set(), "mssql-mock-tds": {"0.1.7"}}
    with patch.object(bump, "cargo_versions", return_value=before), patch.object(
        bump.subprocess, "run"
    ) as cargo:
        assert bump.bump_versions(tmp_path, published) == {}
    cargo.assert_not_called()
    assert "waiting for its mssql-tds dependency" in capsys.readouterr().out


def test_metadata_uses_cargo_json(tmp_path):
    packages = [{"name": crate, "version": "0.1.7"} for crate in bump.CRATES]
    with patch.object(bump.subprocess, "run", return_value=subprocess.CompletedProcess(
        "cargo", 0, stdout=json.dumps({"packages": packages})
    )) as cargo:
        assert bump.cargo_versions(tmp_path) == dict.fromkeys(bump.CRATES, "0.1.7")
    cargo.assert_called_once_with(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
        cwd=tmp_path, check=True, stdout=subprocess.PIPE, text=True,
    )


def test_already_published_target_fails(tmp_path):
    with patch.object(bump, "cargo_versions", side_effect=[
        dict.fromkeys(bump.CRATES, "0.1.7"), dict.fromkeys(bump.CRATES, "0.2.0")
    ]), patch.object(bump.subprocess, "run"):
        with pytest.raises(ValueError, match="already published"):
            bump.bump_versions(tmp_path, dict.fromkeys(bump.CRATES, {"0.1.7", "0.2.0"}))


@pytest.mark.parametrize("operation", ["metadata", "set-version"])
def test_cargo_errors_propagate(tmp_path, operation):
    with patch.object(bump.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "cargo")):
        with pytest.raises(subprocess.CalledProcessError):
            if operation == "metadata":
                bump.cargo_versions(tmp_path)
            else:
                with patch.object(bump, "cargo_versions", return_value=dict.fromkeys(bump.CRATES, "0.1.7")):
                    bump.bump_versions(tmp_path, dict.fromkeys(bump.CRATES, {"0.1.7"}))


def test_registry_response_includes_yanked_and_old_versions():
    response = io.BytesIO(json.dumps({
        "versions": [{"num": "0.2.0", "yanked": False}, {"num": "0.1.0", "yanked": True}]
    }).encode())
    with patch.object(bump, "urlopen", return_value=response) as request:
        assert bump.published_versions("mssql-tds") == {"0.1.0", "0.2.0"}
    assert request.call_args.kwargs["timeout"] == 30
    assert request.call_args.args[0].get_header("User-agent")


@pytest.mark.parametrize("code", [404, 403, 429, 500])
def test_registry_http_errors(code):
    error = HTTPError("https://crates.io", code, "test", {}, None)
    with patch.object(bump, "urlopen", side_effect=error):
        if code == 404:
            assert bump.published_versions("mssql-tds") == set()
        else:
            with pytest.raises(HTTPError):
                bump.published_versions("mssql-tds")


@pytest.mark.parametrize("payload", [b"not json", b"{}", b'{"versions": []}'])
def test_bad_registry_data_fails(payload):
    with patch.object(bump, "urlopen", return_value=io.BytesIO(payload)):
        with pytest.raises((ValueError, KeyError)):
            bump.published_versions("mssql-tds")


def test_registry_timeout_fails():
    with patch.object(bump, "urlopen", side_effect=URLError("timed out")):
        with pytest.raises(URLError):
            bump.published_versions("mssql-tds")


@pytest.fixture
def git_repository(tmp_path):
    root = tmp_path / "checkout"
    root.mkdir()
    remote = tmp_path / "remote.git"

    def git(*args):
        return subprocess.run(
            ["git", "-C", str(root), *args],
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    git("init", "--bare", str(remote))
    git("init", "-b", "main")
    git("config", "commit.gpgsign", "false")
    for crate in bump.CRATES:
        (root / crate).mkdir()
        (root / crate / "Cargo.toml").write_text('version = "0.1.7"\n')
    (root / "unrelated.txt").write_text("Original\n")
    git("add", ".")
    git("-c", "user.name=Test", "-c", "user.email=test@example.invalid",
        "commit", "-m", "Initial manifests")
    git("remote", "add", "origin", str(remote))
    git("push", "-u", "origin", "main")
    return root, remote, git


def test_branch_create_repeat_and_update_only_publish_manifests(git_repository, monkeypatch, capsys):
    root, _, git = git_repository
    base = git("rev-parse", "HEAD")
    (root / "unrelated.txt").write_text("Must not be published\n")
    git("add", "unrelated.txt")
    first = None
    for day, version in enumerate(("0.2.0", "0.2.0", "0.3.0"), start=1):
        git("checkout", "--detach", base)
        monkeypatch.setenv("GIT_AUTHOR_DATE", f"2026-01-0{day}T00:00:00+00:00")
        for crate in bump.CRATES:
            (root / crate / "Cargo.toml").write_text(f'version = "{version}"\n')
        bump.push_bump_branch(root)
        published = git("ls-remote", "--heads", "origin", f"refs/heads/{bump.BUMP_BRANCH}").split()[0]
        assert git("ls-remote", "--heads", "origin", "refs/heads/main").split()[0] == base
        assert set(git("diff", "--name-only", base, published).splitlines()) == {
            f"{crate}/Cargo.toml" for crate in bump.CRATES
        }
        assert git("show", f"{published}:unrelated.txt") == "Original"
        if day == 1:
            first = published
        elif day == 2:
            assert published == first
            assert git("rev-parse", "HEAD") != first
            assert "already up to date" in capsys.readouterr().out
        else:
            assert published != first
            assert 'version = "0.3.0"' in git("show", f"{published}:mssql-tds/Cargo.toml")


@pytest.mark.parametrize("existing", [False, True])
def test_branch_push_rejects_concurrent_creation_or_update(git_repository, existing):
    root, remote, git = git_repository
    base = git("rev-parse", "HEAD")
    if existing:
        (root / "mssql-tds" / "Cargo.toml").write_text('version = "0.2.0"\n')
        bump.push_bump_branch(root)
        git("checkout", "--detach", base)
    (root / "mssql-tds" / "Cargo.toml").write_text('version = "0.3.0"\n')
    ref = f"refs/heads/{bump.BUMP_BRANCH}"
    run = subprocess.run
    raced = False

    def race(args, **kwargs):
        nonlocal raced
        if args[:2] == ["git", "push"]:
            run(["git", "--git-dir", str(remote), "update-ref", ref, base], check=True)
            raced = True
        return run(args, **kwargs)

    with patch.object(bump.subprocess, "run", side_effect=race):
        with pytest.raises(subprocess.CalledProcessError):
            bump.push_bump_branch(root)
    assert raced
    assert git("ls-remote", "--heads", "origin", ref).split()[0] == base


@pytest.fixture
def workflow_environment(tmp_path, monkeypatch):
    monkeypatch.setattr(bump, "__file__", str(tmp_path / "scripts" / "bump.py"))
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    monkeypatch.setenv("DEFAULT_BRANCH", "main")
    monkeypatch.setattr(
        bump.subprocess, "run", Mock(side_effect=AssertionError("Unexpected external command"))
    )
    return tmp_path


def test_main_pushes_branch_before_creating_issue(workflow_environment):
    root = workflow_environment
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "bump_versions", return_value=dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))
    ) as versions, patch.object(bump, "push_bump_branch") as push, patch.object(
        bump, "ensure_bump_issue", return_value=123
    ) as issue:
        issue.side_effect = lambda *args: push.assert_called_once_with(root)
        bump.main()
    versions.assert_called_once_with(root, dict.fromkeys(bump.CRATES, {"0.1.7"}))
    issue.assert_called_once()
    assert set(issue.call_args.args[1]) == set(bump.CRATES)
    for crate in bump.CRATES:
        assert f"`{crate}`: `0.1.7` -> `0.2.0`" in issue.call_args.args[0]


def test_issue_failure_propagates_after_branch_push(workflow_environment):
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "bump_versions", return_value={"mssql-tds": ("0.1.7", "0.2.0")}
    ), patch.object(bump, "push_bump_branch") as push, patch.object(
        bump, "ensure_bump_issue", side_effect=subprocess.CalledProcessError(1, "gh")
    ):
        with pytest.raises(subprocess.CalledProcessError):
            bump.main()
    push.assert_called_once_with(workflow_environment)


def test_main_does_not_partially_bump_on_registry_failure(workflow_environment):
    with patch.object(bump, "published_versions", side_effect=[{"0.1.7"}, URLError("offline")]), patch.object(
        bump, "bump_versions"
    ) as versions:
        with pytest.raises(URLError):
            bump.main()
    versions.assert_not_called()


def test_main_no_changes_needs_no_branch_or_issue(workflow_environment):
    with patch.object(bump, "published_versions", return_value=set()), patch.object(
        bump, "bump_versions", return_value={}
    ), patch.object(bump, "push_bump_branch") as push, patch.object(
        bump, "ensure_bump_issue"
    ) as issue:
        bump.main()
    issue.assert_not_called()
    push.assert_not_called()


def test_cargo_failure_does_not_create_issue(workflow_environment):
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "bump_versions", side_effect=subprocess.CalledProcessError(1, "cargo")
    ), patch.object(bump, "ensure_bump_issue") as issue:
        with pytest.raises(subprocess.CalledProcessError):
            bump.main()
    issue.assert_not_called()


def test_push_failure_does_not_create_issue(workflow_environment):
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "bump_versions", return_value={"mssql-tds": ("0.1.7", "0.2.0")}
    ), patch.object(
        bump, "push_bump_branch", side_effect=subprocess.CalledProcessError(1, "git")
    ), patch.object(bump, "ensure_bump_issue") as issue:
        with pytest.raises(subprocess.CalledProcessError):
            bump.main()
    issue.assert_not_called()


def issue_pages(*pages):
    return json.dumps([
        {"data": {"repository": {"issues": {"nodes": page}}}} for page in pages
    ])


def test_issue_created_once_then_reused_and_updated(monkeypatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    monkeypatch.setenv("DEFAULT_BRANCH", "release/next")
    stored = []
    writes = []

    def github(args, **kwargs):
        assert kwargs["check"] is True
        assert kwargs["stdout"] is subprocess.PIPE
        assert "stderr" not in kwargs  # API diagnostics remain visible in the job log.
        endpoint = "repos/microsoft/mssql-rs/issues"
        if "--method" not in args:
            assert args[:3] == ["gh", "api", "graphql"]
            assert args[5:] == ["-f", "owner=microsoft", "-f", "name=mssql-rs", "--paginate", "--slurp"]
            assert "issues(first: 100, after: $endCursor, states: OPEN)" in args[4]
            assert "pageInfo { hasNextPage endCursor }" in args[4]
            # The matching issue is on the second page.
            pages = issue_pages([
                {"number": 10, "body": "Unrelated"},
                {"number": 11, "body": None},
            ], stored)
            return subprocess.CompletedProcess(args, 0, stdout=pages)
        method = args[args.index("--method") + 1]
        writes.append(method)
        assert args == [
            "gh", "api", endpoint if method == "POST" else f"{endpoint}/123",
            "--method", method, "--input", "-"
        ]
        payload = json.loads(kwargs["input"])
        if method == "POST":
            assert not stored
            assert payload["title"] == "Bump released crates to the next minor version"
            stored.append({"number": 123, **payload})
        else:
            assert method == "PATCH"
            stored[0].update(payload)
        return subprocess.CompletedProcess(args, 0, stdout=json.dumps(stored[0]))

    with patch.object(bump.subprocess, "run", side_effect=github):
        assert bump.ensure_bump_issue("First bump", ["mssql-tds"]) == 123
        assert bump.ensure_bump_issue("First bump", ["mssql-tds"]) == 123
        assert writes == ["POST"]
        assert bump.ensure_bump_issue("Both bumps", bump.CRATES) == 123
    assert writes == ["POST", "PATCH"]
    assert len(stored) == 1
    assert stored[0]["body"].startswith(bump.ISSUE_MARKER)
    assert "Both bumps" in stored[0]["body"]
    assert "First bump" not in stored[0]["body"]
    assert (
        "[Create PR](https://github.com/microsoft/mssql-rs/compare/"
        "release%2Fnext...automation%2Fbump-released-crate-versions?expand=1)"
    ) in stored[0]["body"]
    assert "Assign this issue to Copilot through **Assignees**" in stored[0]["body"]
    assert "open a PR against `release/next`" in stored[0]["body"]
    assert "Do not bump the versions again." in stored[0]["body"]
    assert "Fixes #<this issue number>" in stored[0]["body"]
    assert "assignees" not in stored[0]
    for crate in bump.CRATES:
        assert f"`{crate}`" in stored[0]["body"]
    template = yaml.safe_load(
        (ROOT / ".github" / "ISSUE_TEMPLATE" / "feature_request.yml").read_text()
    )
    for field in template["body"]:
        if field["type"] != "markdown":
            assert f"### {field['attributes']['label']}\n" in stored[0]["body"]


@pytest.mark.parametrize("operation", ["list", "create", "update"])
def test_issue_api_errors_propagate(monkeypatch, operation):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    monkeypatch.setenv("DEFAULT_BRANCH", "main")
    failure = subprocess.CalledProcessError(1, "gh")
    pages = [[{"number": 123, "body": bump.ISSUE_MARKER}]] if operation == "update" else [[]]
    results = [failure] if operation == "list" else [
        subprocess.CompletedProcess("gh", 0, stdout=issue_pages(*pages)), failure
    ]
    with patch.object(bump.subprocess, "run", side_effect=results) as github:
        with pytest.raises(subprocess.CalledProcessError):
            bump.ensure_bump_issue("Bump", bump.CRATES)
    assert github.call_count == len(results)


def test_duplicate_tracking_issues_fail_without_writing(monkeypatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    pages = [[{"number": number, "body": bump.ISSUE_MARKER} for number in (123, 456)]]
    with patch.object(bump.subprocess, "run", return_value=subprocess.CompletedProcess(
        "gh", 0, stdout=issue_pages(*pages)
    )) as github:
        with pytest.raises(ValueError, match="Multiple open"):
            bump.ensure_bump_issue("Bump", bump.CRATES)
    github.assert_called_once()


def test_workflow_scope_and_branch_issue_permissions():
    workflow = yaml.safe_load(
        (ROOT / ".github" / "workflows" / "bump-released-crate-versions.yml").read_text()
    )
    triggers = workflow.get("on", workflow.get(True))
    assert triggers["schedule"] == [{"cron": "23 8 * * *"}]
    assert "workflow_dispatch" in triggers
    assert workflow["permissions"] == {}
    assert workflow["jobs"]["bump"]["permissions"] == {
        "contents": "write", "issues": "write"
    }
    assert workflow["concurrency"]["cancel-in-progress"] is False
    steps = workflow["jobs"]["bump"]["steps"]
    assert steps[0]["with"]["ref"] == "${{ github.event.repository.default_branch }}"
    assert steps[0]["with"]["persist-credentials"] is True
    assert steps[1]["id"] == "cadence"
    install = steps[2]["run"]
    assert "cargo install cargo-edit --version " in install
    assert "--locked --no-default-features --features set-version" in install
    assert '--root "$RUNNER_TEMP/cargo-edit"' in install
    assert 'echo "$RUNNER_TEMP/cargo-edit/bin" >> "$GITHUB_PATH"' in install
    for step in steps[2:]:
        assert step["if"] == "steps.cadence.outputs.due == 'true'"
    assert steps[3]["run"] == "python3 scripts/bump-released-crate-versions.py"
    assert steps[3]["env"] == {
        "GH_TOKEN": "${{ github.token }}",
        "DEFAULT_BRANCH": "${{ github.event.repository.default_branch }}",
    }
    assert len(steps) == 4
    assert all(
        len(step["uses"].split("@")[1]) == 40 for step in steps if "uses" in step
    )


@pytest.mark.parametrize("event", ["schedule", "workflow_dispatch"])
def test_workflow_cadence_across_month_year_and_leap_day(tmp_path, monkeypatch, event):
    workflow = yaml.safe_load(
        (ROOT / ".github" / "workflows" / "bump-released-crate-versions.yml").read_text()
    )
    script = workflow["jobs"]["bump"]["steps"][1]["run"]
    code = script.removeprefix("python3 - <<'PY'\n").removesuffix("PY\n")
    output = tmp_path / "output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setenv("GITHUB_EVENT_NAME", event)
    start = datetime(2023, 12, 25, tzinfo=timezone.utc)
    with patch("datetime.datetime") as clock:
        for day in range(440):
            clock.now.return_value = start + timedelta(days=day)
            exec(code, {})
    due = [day for day, line in enumerate(output.read_text().splitlines()) if line == "due=true"]
    if event == "schedule":
        assert len(due) in (146, 147)
        assert all(right - left == 3 for left, right in zip(due, due[1:]))
    else:
        assert due == list(range(440))

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import importlib.util
import io
import json
import subprocess
import tomllib
from datetime import date, timedelta
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


@pytest.fixture
def manifests(tmp_path):
    for crate in bump.CRATES:
        directory = tmp_path / crate
        directory.mkdir()
        (directory / "Cargo.toml").write_text(
            '[package]\nname = "' + crate + '"\nversion = "0.1.7"\n'
            '[dependencies]\nother = "0.1.7"\n'
            + (
                'mssql-tds = { path = "../mssql-tds", version = "0.1.7", '
                'default-features = false }\n'
                if crate == "mssql-mock-tds" else ""
            ),
            encoding="utf-8",
        )
    return tmp_path


@pytest.mark.parametrize("released", [(), ("mssql-tds",), ("mssql-mock-tds",), bump.CRATES])
def test_independent_bumps_and_repeat(manifests, released):
    versions = {
        crate: {"0.1.7", "0.0.1"} if crate in released else {"0.1.6"}
        for crate in bump.CRATES
    }
    contents, changes = bump.prepare_bump(manifests, versions)
    assert set(changes) == set(released)
    for crate, content in contents.items():
        manifest = tomllib.loads(content)
        assert manifest["package"]["version"] == (
            "0.2.0" if crate in released else "0.1.7"
        )
        assert manifest["dependencies"]["other"] == "0.1.7"
        (manifests / crate / "Cargo.toml").write_text(content, encoding="utf-8")
    dependency = tomllib.loads(contents["mssql-mock-tds"])["dependencies"]["mssql-tds"]
    assert dependency == {
        "path": "../mssql-tds",
        "version": "0.2.0" if "mssql-tds" in released else "0.1.7",
        "default-features": False,
    }
    repeated, changes = bump.prepare_bump(manifests, versions)
    assert changes == {}
    assert repeated == contents


def test_unpublished_crates_are_unchanged(manifests):
    before = {crate: (manifests / crate / "Cargo.toml").read_bytes() for crate in bump.CRATES}
    _, changes = bump.prepare_bump(manifests, dict.fromkeys(bump.CRATES, set()))
    assert changes == {}
    assert before == {
        crate: (manifests / crate / "Cargo.toml").read_bytes() for crate in bump.CRATES
    }


def test_already_published_target_fails_before_writing(manifests):
    with pytest.raises(ValueError, match="already published"):
        bump.prepare_bump(manifests, dict.fromkeys(bump.CRATES, {"0.1.7", "0.2.0"}))
    for crate in bump.CRATES:
        assert tomllib.loads((manifests / crate / "Cargo.toml").read_text())[
            "package"
        ]["version"] == "0.1.7"


@pytest.mark.parametrize("version", ["1.9.8", "1.9.8-rc.1", "1.9.8+build.7"])
def test_minor_bump_resets_patch_and_suffix(version):
    assert bump.next_minor(version) == "1.10.0"


def test_three_day_cadence_across_month_year_and_leap_day():
    start = date(2023, 12, 25)
    due = [
        start + timedelta(days=day) for day in range(440)
        if bump.is_due(start + timedelta(days=day))
    ]
    assert len(due) in (146, 147)
    assert all((right - left).days == 3 for left, right in zip(due, due[1:]))


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
def workflow_environment(manifests, monkeypatch):
    template = manifests / ".github" / "PULL_REQUEST_TEMPLATE.md"
    template.parent.mkdir()
    template.write_text(
        (ROOT / ".github" / "PULL_REQUEST_TEMPLATE.md").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    monkeypatch.setattr(bump, "__file__", str(manifests / "scripts" / "bump.py"))
    monkeypatch.setenv("GITHUB_EVENT_NAME", "workflow_dispatch")
    monkeypatch.setenv("RUNNER_TEMP", str(manifests))
    monkeypatch.setenv("GITHUB_OUTPUT", str(manifests / "output"))
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    monkeypatch.setattr(
        bump.subprocess, "run", Mock(side_effect=AssertionError("Unexpected GitHub call"))
    )
    return manifests


def test_main_writes_manifests_template_and_output(workflow_environment):
    root = workflow_environment
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "ensure_bump_issue", return_value=123
    ) as issue:
        bump.main()
        first_body = (root / "crate-version-bump-pr.md").read_text()
        for crate in bump.CRATES:
            assert f"`{crate}`: `0.1.7` -> `0.2.0`" in first_body
            assert tomllib.loads((root / crate / "Cargo.toml").read_text())[
                "package"
            ]["version"] == "0.2.0"
        assert "## Related Issues\n\nFixes #123" in first_body
        assert "- [ ] `cargo bclippy` passes" in first_body
        assert "<!--" in first_body
        bump.main()
    issue.assert_called_once()
    assert set(issue.call_args.args[1]) == set(bump.CRATES)
    assert (root / "output").read_text() == "due=true\ndue=true\n"
    for crate in bump.CRATES:
        assert tomllib.loads((root / crate / "Cargo.toml").read_text())[
            "package"
        ]["version"] == "0.2.0"


def test_issue_failure_stops_before_writing(workflow_environment):
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "ensure_bump_issue", side_effect=subprocess.CalledProcessError(1, "gh")
    ):
        with pytest.raises(subprocess.CalledProcessError):
            bump.main()
    for crate in bump.CRATES:
        assert tomllib.loads((workflow_environment / crate / "Cargo.toml").read_text())[
            "package"
        ]["version"] == "0.1.7"
    assert not (workflow_environment / "output").exists()


def test_main_does_not_partially_bump_on_registry_failure(workflow_environment):
    with patch.object(bump, "published_versions", side_effect=[{"0.1.7"}, URLError("offline")]):
        with pytest.raises(URLError):
            bump.main()
    for crate in bump.CRATES:
        assert tomllib.loads((workflow_environment / crate / "Cargo.toml").read_text())[
            "package"
        ]["version"] == "0.1.7"
    assert not (workflow_environment / "output").exists()


def test_main_no_changes_needs_no_issue_and_allows_pr_cleanup(workflow_environment):
    with patch.object(bump, "published_versions", return_value=set()), patch.object(
        bump, "ensure_bump_issue"
    ) as issue:
        bump.main()
    issue.assert_not_called()
    assert (workflow_environment / "output").read_text() == "due=true\n"
    assert (workflow_environment / "crate-version-bump-pr.md").is_file()


def test_schedule_skips_registry_and_pr_on_off_days(workflow_environment, monkeypatch):
    monkeypatch.setenv("GITHUB_EVENT_NAME", "schedule")
    with patch.object(bump, "is_due", return_value=False), patch.object(
        bump, "published_versions"
    ) as lookup:
        bump.main()
    lookup.assert_not_called()
    assert not (workflow_environment / "output").exists()


def test_issue_created_once_then_reused_and_updated(monkeypatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    stored = []
    writes = []

    def github(args, **kwargs):
        assert kwargs["check"] is True
        assert kwargs["stdout"] is subprocess.PIPE
        assert "stderr" not in kwargs  # API diagnostics remain visible in the job log.
        endpoint = "repos/microsoft/mssql-rs/issues"
        if "--method" not in args:
            assert args == [
                "gh", "api", f"{endpoint}?state=open&per_page=100", "--paginate", "--slurp"
            ]
            # The real issue is on the second page; PRs and unrelated issues must not match.
            pages = [[
                {"number": 10, "body": bump.ISSUE_MARKER, "pull_request": {}},
                {"number": 11, "body": None},
            ], stored]
            return subprocess.CompletedProcess(args, 0, stdout=json.dumps(pages))
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
        # Also covers a rerun after issue creation succeeded but PR creation failed.
        assert bump.ensure_bump_issue("First bump", ["mssql-tds"]) == 123
        assert writes == ["POST"]
        assert bump.ensure_bump_issue("Both bumps", bump.CRATES) == 123
    assert writes == ["POST", "PATCH"]
    assert len(stored) == 1
    assert stored[0]["body"].startswith(bump.ISSUE_MARKER)
    assert "Both bumps" in stored[0]["body"]
    assert "First bump" not in stored[0]["body"]
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
    failure = subprocess.CalledProcessError(1, "gh")
    pages = [[{"number": 123, "body": bump.ISSUE_MARKER}]] if operation == "update" else [[]]
    results = [failure] if operation == "list" else [
        subprocess.CompletedProcess("gh", 0, stdout=json.dumps(pages)), failure
    ]
    with patch.object(bump.subprocess, "run", side_effect=results) as github:
        with pytest.raises(subprocess.CalledProcessError):
            bump.ensure_bump_issue("Bump", bump.CRATES)
    assert github.call_count == len(results)


def test_duplicate_tracking_issues_fail_without_writing(monkeypatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    pages = [[{"number": number, "body": bump.ISSUE_MARKER} for number in (123, 456)]]
    with patch.object(bump.subprocess, "run", return_value=subprocess.CompletedProcess(
        "gh", 0, stdout=json.dumps(pages)
    )) as github:
        with pytest.raises(ValueError, match="Multiple open"):
            bump.ensure_bump_issue("Bump", bump.CRATES)
    github.assert_called_once()


def test_workflow_scope_and_pr_safety():
    workflow = yaml.safe_load(
        (ROOT / ".github" / "workflows" / "bump-released-crate-versions.yml").read_text()
    )
    triggers = workflow.get("on", workflow.get(True))
    assert triggers["schedule"] == [{"cron": "23 8 * * *"}]
    assert "workflow_dispatch" in triggers
    assert workflow["permissions"] == {}
    assert workflow["jobs"]["bump"]["permissions"] == {
        "contents": "write", "issues": "write", "pull-requests": "write"
    }
    assert workflow["concurrency"]["cancel-in-progress"] is False
    steps = workflow["jobs"]["bump"]["steps"]
    assert steps[0]["with"]["ref"] == "${{ github.event.repository.default_branch }}"
    assert steps[0]["with"]["persist-credentials"] is False
    assert steps[1]["run"] == "python3 scripts/bump-released-crate-versions.py"
    assert steps[1]["env"] == {
        "GH_TOKEN": "${{ secrets.CRATE_VERSION_BUMP_TOKEN || github.token }}"
    }
    pr = steps[2]
    assert pr["if"] == "steps.check.outputs.due == 'true'"
    assert pr["with"]["branch"] == "automation/bump-released-crate-versions"
    assert pr["with"]["draft"] == "always-true"
    assert set(pr["with"]["add-paths"].split()) == {
        f"{crate}/Cargo.toml" for crate in bump.CRATES
    }
    assert all(
        len(step["uses"].split("@")[1]) == 40 for step in steps if "uses" in step
    )

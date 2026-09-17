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


@pytest.mark.parametrize("released", [(), ("mssql-tds",), ("mssql-mock-tds",), bump.CRATES])
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
def workflow_environment(tmp_path, monkeypatch):
    template = tmp_path / ".github" / "PULL_REQUEST_TEMPLATE.md"
    template.parent.mkdir()
    template.write_text(
        (ROOT / ".github" / "PULL_REQUEST_TEMPLATE.md").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    monkeypatch.setattr(bump, "__file__", str(tmp_path / "scripts" / "bump.py"))
    monkeypatch.setenv("RUNNER_TEMP", str(tmp_path))
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    monkeypatch.setattr(
        bump.subprocess, "run", Mock(side_effect=AssertionError("Unexpected external command"))
    )
    return tmp_path


def test_main_links_issue_and_preserves_template(workflow_environment):
    root = workflow_environment
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "bump_versions", return_value=dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))
    ) as versions, patch.object(
        bump, "ensure_bump_issue", return_value=123
    ) as issue:
        bump.main()
        first_body = (root / "crate-version-bump-pr.md").read_text()
        for crate in bump.CRATES:
            assert f"`{crate}`: `0.1.7` -> `0.2.0`" in first_body
        assert "## Related Issues\n\nFixes #123" in first_body
        assert "- [ ] `cargo bclippy` passes" in first_body
        assert "<!--" in first_body
    versions.assert_called_once_with(root, dict.fromkeys(bump.CRATES, {"0.1.7"}))
    issue.assert_called_once()
    assert set(issue.call_args.args[1]) == set(bump.CRATES)


def test_issue_failure_stops_before_pr_body(workflow_environment):
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "bump_versions", return_value={"mssql-tds": ("0.1.7", "0.2.0")}
    ), patch.object(
        bump, "ensure_bump_issue", side_effect=subprocess.CalledProcessError(1, "gh")
    ):
        with pytest.raises(subprocess.CalledProcessError):
            bump.main()
    assert not (workflow_environment / "crate-version-bump-pr.md").exists()


def test_main_does_not_partially_bump_on_registry_failure(workflow_environment):
    with patch.object(bump, "published_versions", side_effect=[{"0.1.7"}, URLError("offline")]), patch.object(
        bump, "bump_versions"
    ) as versions:
        with pytest.raises(URLError):
            bump.main()
    versions.assert_not_called()
    assert not (workflow_environment / "crate-version-bump-pr.md").exists()


def test_main_no_changes_needs_no_issue_and_allows_pr_cleanup(workflow_environment):
    with patch.object(bump, "published_versions", return_value=set()), patch.object(
        bump, "bump_versions", return_value={}
    ), patch.object(
        bump, "ensure_bump_issue"
    ) as issue:
        bump.main()
    issue.assert_not_called()
    assert (workflow_environment / "crate-version-bump-pr.md").is_file()


def test_cargo_failure_does_not_create_issue(workflow_environment):
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "bump_versions", side_effect=subprocess.CalledProcessError(1, "cargo")
    ), patch.object(bump, "ensure_bump_issue") as issue:
        with pytest.raises(subprocess.CalledProcessError):
            bump.main()
    issue.assert_not_called()
    assert not (workflow_environment / "crate-version-bump-pr.md").exists()


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
        "GH_TOKEN": "${{ secrets.CRATE_VERSION_BUMP_TOKEN || github.token }}"
    }
    pr = steps[4]
    assert pr["with"]["branch"] == "automation/bump-released-crate-versions"
    assert pr["with"]["draft"] == "always-true"
    assert set(pr["with"]["add-paths"].split()) == {
        f"{crate}/Cargo.toml" for crate in bump.CRATES
    }
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

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


@pytest.mark.parametrize(("released", "expected"), [
    ((), {}),
    (("mssql-tds",), dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))),
    (bump.CRATES, dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))),
])
def test_plans_released_crates(tmp_path, released, expected):
    current = dict.fromkeys(bump.CRATES, "0.1.7")
    published = {crate: {"0.1.7"} if crate in released else set() for crate in bump.CRATES}
    with patch.object(bump, "cargo_versions", return_value=current):
        assert bump.planned_bumps(tmp_path, published) == expected


def test_mock_bump_waits_for_published_core(tmp_path, capsys):
    current = dict.fromkeys(bump.CRATES, "0.1.7")
    published = {"mssql-tds": set(), "mssql-mock-tds": {"0.1.7"}}
    with patch.object(bump, "cargo_versions", return_value=current):
        assert bump.planned_bumps(tmp_path, published) == {}
    assert "waiting for its mssql-tds dependency" in capsys.readouterr().out


def test_mock_target_follows_core_target(tmp_path):
    current = {"mssql-tds": "0.2.0", "mssql-mock-tds": "0.1.0"}
    published = {"mssql-tds": {"0.2.0"}, "mssql-mock-tds": set()}
    with patch.object(bump, "cargo_versions", return_value=current):
        assert bump.planned_bumps(tmp_path, published) == {
            "mssql-tds": ("0.2.0", "0.3.0"),
            "mssql-mock-tds": ("0.1.0", "0.3.0"),
        }


def test_already_published_target_fails(tmp_path):
    with patch.object(bump, "cargo_versions", return_value=dict.fromkeys(bump.CRATES, "0.1.7")):
        with pytest.raises(ValueError, match="already published"):
            bump.planned_bumps(tmp_path, dict.fromkeys(bump.CRATES, {"0.1.7", "0.2.0"}))


@pytest.mark.parametrize(
    ("current", "expected"),
    [("0.1.7", "0.2.0"), ("1.9.3", "1.10.0")],
)
def test_next_minor(current, expected):
    assert bump.next_minor(current) == expected


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
    monkeypatch.setattr(bump, "__file__", str(tmp_path / "scripts" / "bump.py"))
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")

    def unexpected_command(args, **kwargs):
        if args[:3] == ["gh", "pr", "list"]:
            return subprocess.CompletedProcess(args, 0, stdout="[]")
        raise AssertionError("Unexpected external command")

    monkeypatch.setattr(bump.subprocess, "run", Mock(side_effect=unexpected_command))
    return tmp_path


def test_has_open_bump_pr(workflow_environment):
    changes = {"mssql-tds": ("0.1.7", "0.2.0")}
    with patch.object(
        bump.subprocess, "run",
        return_value=subprocess.CompletedProcess(
            "gh", 0,
            stdout=json.dumps([
                {"number": 584, "title": "Fix mssql-tds docs", "body": "Mentions mssql-tds only"},
                {"number": 597, "title": "Bump mssql-tds", "body": "- `mssql-tds`: `0.1.7` -> `0.2.0`"},
            ]),
        ),
    ) as gh:
        assert bump.has_open_bump_pr(workflow_environment, changes)
    gh.assert_called_once_with(
        [
            "gh", "pr", "list", "--repo", "microsoft/mssql-rs",
            "--state", "open", "--json", "number,title,body", "--limit", "100",
        ],
        cwd=workflow_environment, check=True, stdout=subprocess.PIPE, text=True,
    )


def test_open_bump_pr_ignores_unrelated_crate_mentions(workflow_environment):
    with patch.object(
        bump.subprocess, "run",
        return_value=subprocess.CompletedProcess(
            "gh", 0, stdout='[{"number": 584, "title": "Fix mssql-tds docs", "body": ""}]'
        ),
    ):
        assert not bump.has_open_bump_pr(
            workflow_environment, {"mssql-tds": ("0.1.7", "0.2.0")}
        )


def test_open_bump_pr_requires_every_selected_crate(workflow_environment):
    changes = dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))
    with patch.object(
        bump.subprocess, "run",
        return_value=subprocess.CompletedProcess(
            "gh", 0,
            stdout=json.dumps([
                {
                    "number": 600,
                    "title": "Bump mssql-tds",
                    "body": "- `mssql-tds`: `0.1.7` -> `0.2.0`",
                }
            ]),
        ),
    ):
        assert not bump.has_open_bump_pr(workflow_environment, changes)


def test_open_bump_pr_requires_exact_transition(workflow_environment):
    with patch.object(
        bump.subprocess, "run",
        return_value=subprocess.CompletedProcess(
            "gh", 0,
            stdout=json.dumps([
                {
                    "number": 601,
                    "title": "Bump mssql-tds to 0.2.0",
                    "body": "- `mssql-tds`: `0.1.6` -> `0.2.0`",
                }
            ]),
        ),
    ):
        assert not bump.has_open_bump_pr(
            workflow_environment, {"mssql-tds": ("0.1.7", "0.2.0")}
        )


def test_open_bump_pr_requires_summary_line_format(workflow_environment):
    with patch.object(
        bump.subprocess, "run",
        return_value=subprocess.CompletedProcess(
            "gh", 0,
            stdout=json.dumps([
                {
                    "number": 602,
                    "title": "Compatibility notes",
                    "body": "Compatibility notes for mssql-tds 0.1.7 and 0.2.0",
                }
            ]),
        ),
    ):
        assert not bump.has_open_bump_pr(
            workflow_environment, {"mssql-tds": ("0.1.7", "0.2.0")}
        )


def test_main_creates_issue_for_planned_bumps(workflow_environment):
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "planned_bumps", return_value=dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))
    ) as plan, patch.object(bump, "has_open_bump_pr", return_value=False) as prs, patch.object(
        bump, "ensure_bump_issue", return_value=123
    ) as issue:
        bump.main()
    plan.assert_called_once_with(workflow_environment, dict.fromkeys(bump.CRATES, {"0.1.7"}))
    prs.assert_called_once()
    issue.assert_called_once()
    assert set(issue.call_args.args[1]) == set(bump.CRATES)
    for crate in bump.CRATES:
        assert f"- `{crate}`: `0.1.7` -> `0.2.0`" in issue.call_args.args[0]


def test_main_skips_issue_when_bump_pr_exists(workflow_environment):
    with patch.object(bump, "published_versions", return_value={"0.1.7"}), patch.object(
        bump, "planned_bumps", return_value={"mssql-tds": ("0.1.7", "0.2.0")}
    ), patch.object(bump, "has_open_bump_pr", return_value=True), patch.object(
        bump, "ensure_bump_issue"
    ) as issue:
        bump.main()
    issue.assert_not_called()


def test_main_does_not_plan_on_registry_failure(workflow_environment):
    with patch.object(bump, "published_versions", side_effect=[{"0.1.7"}, URLError("offline")]), patch.object(
        bump, "planned_bumps"
    ) as plan:
        with pytest.raises(URLError):
            bump.main()
    plan.assert_not_called()


def test_main_no_changes_creates_no_issue(workflow_environment):
    with patch.object(bump, "published_versions", return_value=set()), patch.object(
        bump, "planned_bumps", return_value={}
    ), patch.object(bump, "has_open_bump_pr") as prs, patch.object(
        bump, "ensure_bump_issue"
    ) as issue:
        bump.main()
    prs.assert_not_called()
    issue.assert_not_called()


def issue_pages(*pages):
    return json.dumps([
        {"data": {"repository": {"issues": {"nodes": page}}}} for page in pages
    ])


def test_issue_created_once_then_reused_and_updated(monkeypatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    stored = []
    writes = []
    labels_created = []

    def github(args, **kwargs):
        assert kwargs["check"] is True
        assert kwargs["stdout"] is subprocess.PIPE
        if args[:3] == ["gh", "label", "create"]:
            assert args[3] == bump.ISSUE_LABEL
            assert args[args.index("--repo") + 1] == "microsoft/mssql-rs"
            assert "--force" in args
            labels_created.append(args[3])
            return subprocess.CompletedProcess(args, 0, stdout="")
        endpoint = "repos/microsoft/mssql-rs/issues"
        if "--method" not in args:
            assert args[:3] == ["gh", "api", "graphql"]
            assert args[5:] == [
                "-f", "owner=microsoft", "-f", "name=mssql-rs",
                "-f", f"label={bump.ISSUE_LABEL}", "--paginate", "--slurp",
            ]
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
            assert payload["labels"] == [bump.ISSUE_LABEL]
            stored.append({"number": 123, **payload})
        else:
            assert method == "PATCH"
            assert "labels" not in payload
            stored[0].update(payload)
        return subprocess.CompletedProcess(args, 0, stdout=json.dumps(stored[0]))

    first = {"mssql-tds": ("0.1.7", "0.2.0")}
    both = dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))
    with patch.object(bump.subprocess, "run", side_effect=github):
        assert bump.ensure_bump_issue("- `mssql-tds`: `0.1.7` -> `0.2.0`", first) == 123
        assert bump.ensure_bump_issue("- `mssql-tds`: `0.1.7` -> `0.2.0`", first) == 123
        assert writes == ["POST"]
        assert bump.ensure_bump_issue("Both bumps", both) == 123
    assert writes == ["POST", "PATCH"]
    assert labels_created == [bump.ISSUE_LABEL]
    assert len(stored) == 1
    assert stored[0]["body"].startswith(bump.ISSUE_MARKER)
    assert "Both bumps" in stored[0]["body"]
    assert "First bump" not in stored[0]["body"]
    assert "mssql-tds/Cargo.toml" in stored[0]["body"]
    assert 'version = "0.2.0"' in stored[0]["body"]
    assert 'mssql-tds = { path = "../mssql-tds", version = "0.2.0", default-features = false }' in stored[0]["body"]
    assert "Run `cargo bfmt`, `cargo bclippy`, and `cargo btest`." in stored[0]["body"]
    assert "Fixes #<this issue number>" in stored[0]["body"]
    assert "plus the version summary above" in stored[0]["body"]
    assert "put maintainer notes in issue comments" in stored[0]["body"]
    template = yaml.safe_load(
        (ROOT / ".github" / "ISSUE_TEMPLATE" / "feature_request.yml").read_text()
    )
    for field in template["body"]:
        if field["type"] != "markdown":
            assert f"### {field['attributes']['label']}\n" in stored[0]["body"]


@pytest.mark.parametrize("operation", ["list", "label", "create", "update"])
def test_issue_api_errors_propagate(monkeypatch, operation):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    failure = subprocess.CalledProcessError(1, "gh")
    pages = [[{"number": 123, "body": bump.ISSUE_MARKER}]] if operation == "update" else [[]]
    listed = subprocess.CompletedProcess("gh", 0, stdout=issue_pages(*pages))
    results = {
        "list": [failure],
        "label": [listed, failure],
        "create": [listed, subprocess.CompletedProcess("gh", 0, stdout=""), failure],
        "update": [listed, failure],
    }[operation]
    with patch.object(bump.subprocess, "run", side_effect=results) as github:
        with pytest.raises(subprocess.CalledProcessError):
            bump.ensure_bump_issue("Bump", dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0")))
    assert github.call_count == len(results)


def test_duplicate_tracking_issues_fail_without_writing(monkeypatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    pages = [[{"number": number, "body": bump.ISSUE_MARKER} for number in (123, 456)]]
    with patch.object(bump.subprocess, "run", return_value=subprocess.CompletedProcess(
        "gh", 0, stdout=issue_pages(*pages)
    )) as github:
        with pytest.raises(ValueError, match="Multiple open"):
            bump.ensure_bump_issue("Bump", dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0")))
    github.assert_called_once()


def test_workflow_scope_and_issue_permissions():
    workflow = yaml.safe_load(
        (ROOT / ".github" / "workflows" / "bump-released-crate-versions.yml").read_text()
    )
    triggers = workflow.get("on", workflow.get(True))
    assert triggers["schedule"] == [{"cron": "23 8 * * *"}]
    assert "workflow_dispatch" in triggers
    assert workflow["permissions"] == {}
    assert workflow["jobs"]["bump"]["permissions"] == {
        "contents": "read", "issues": "write", "pull-requests": "read"
    }
    assert workflow["concurrency"]["cancel-in-progress"] is False
    steps = workflow["jobs"]["bump"]["steps"]
    assert steps[0]["with"]["ref"] == "${{ github.event.repository.default_branch }}"
    assert steps[0]["with"]["persist-credentials"] is False
    assert steps[1]["id"] == "cadence"
    for step in steps[2:]:
        assert step["if"] == "steps.cadence.outputs.due == 'true'"
    assert steps[2]["run"] == "python3 scripts/bump-released-crate-versions.py"
    assert steps[2]["env"] == {"GH_TOKEN": "${{ github.token }}"}
    assert len(steps) == 3
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

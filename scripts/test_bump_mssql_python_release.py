# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import base64
import importlib.util
import io
import json
import os
import shutil
import subprocess
from pathlib import Path
from unittest.mock import Mock, patch
from urllib.error import HTTPError, URLError

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "bump_mssql_python", ROOT / "scripts" / "bump-mssql-python-release.py"
)
bump = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bump)
PIPELINE = ROOT / ".pipeline" / "bump-mssql-python-release.yml"
PYPI_RELEASE = ROOT / ".pipeline" / "OneBranch" / "PyPIRelease.yml"


def _working_bash():
    path = shutil.which("bash")
    if not path:
        return None
    try:
        probe = subprocess.run(
            [path, "-c", 'V=ok; printf "%s" "$V"'],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return path if probe.returncode == 0 and probe.stdout.strip() == "ok" else None


BASH = _working_bash()


@pytest.fixture(autouse=True)
def environment(monkeypatch):
    monkeypatch.setenv("GH_TOKEN", "ghs_test")
    monkeypatch.setenv("DEFAULT_BRANCH", "main")
    monkeypatch.setenv("MSSQL_PYTHON_REPOSITORY", "microsoft/mssql-python")


def response(payload, status=200):
    value = Mock()
    value.status = status
    value.read.return_value = json.dumps(payload).encode()
    value.__enter__ = Mock(return_value=value)
    value.__exit__ = Mock(return_value=False)
    return value


def target_repo(tmp_path, python="0.3.0", nuget="0.3.0"):
    for path, value in ((bump.PYPI_PIN, python), (bump.NUGET_PIN, nuget)):
        target = tmp_path / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(value, encoding="utf-8")
    template = tmp_path / bump.PR_TEMPLATE
    template.parent.mkdir(parents=True, exist_ok=True)
    template.write_text(
        "### Work Item / Issue Reference\n"
        "> AB#<WORK_ITEM_ID>\n"
        "> GitHub Issue: #<ISSUE_NUMBER>\n"
        "### Summary\n"
        "<!-- Insert your summary of changes below. Minimum 10 characters required. -->\n",
        encoding="utf-8",
    )
    return tmp_path


def worker_steps(pipeline):
    deployment = pipeline["stages"][0]["jobs"][0]
    return deployment["strategy"]["runOnce"]["deploy"]["steps"]


def shell_script(step):
    return step["script"].replace("\r\n", "\n").replace("\r", "\n")


@pytest.mark.parametrize(
    ("current_python", "current_nuget", "available", "expected"),
    [
        ("0.3.0", "0.3.0", True, {bump.PYPI_PIN, bump.NUGET_PIN}),
        ("0.3.0", "0.3.0", False, {bump.PYPI_PIN}),
        ("0.4.0", "0.3.0", True, {bump.NUGET_PIN}),
        ("0.4.0", "0.3.0", False, set()),
        ("0.5.0", "0.3.0", True, {bump.NUGET_PIN}),
        ("0.5.0", "0.5.0", True, set()),
    ],
)
def test_planned_changes(tmp_path, current_python, current_nuget, available, expected):
    target_repo(tmp_path, current_python, current_nuget)
    changes = bump.planned_changes(tmp_path, "0.4.0", available)
    assert set(changes) == expected


def test_stale_release_never_downgrades_either_pin(tmp_path):
    target_repo(tmp_path, "0.5.0", "0.6.0")
    assert bump.planned_changes(tmp_path, "0.4.0", True) == {}


def test_release_state_requires_pypi_and_checks_exact_nuget_version():
    pypi = response({"info": {"version": "0.4.0"}})
    nuget = response({"versions": ["0.3.0", "0.4.0-nightly.1", "0.4.0"]})
    with patch.object(bump, "urlopen", side_effect=[pypi, nuget]):
        assert bump.release_state("0.4.0") is True


def test_release_state_does_not_treat_prerelease_nuget_as_clean_release():
    with patch.object(
        bump,
        "urlopen",
        side_effect=[
            response({"info": {"version": "0.4.0"}}),
            response({"versions": ["0.4.0-nightly.20261016"]}),
        ],
    ):
        assert bump.release_state("0.4.0") is False


def test_release_state_treats_missing_nuget_package_as_unavailable():
    missing_nuget = HTTPError("https://pkgs.dev.azure.com", 404, "missing", {}, io.BytesIO())
    with patch.object(
        bump,
        "urlopen",
        side_effect=[response({"info": {"version": "0.4.0"}}), missing_nuget],
    ):
        assert bump.release_state("0.4.0") is False


def test_missing_pypi_release_fails_before_writing():
    error = HTTPError("https://pypi.org", 404, "missing", {}, io.BytesIO())
    with patch.object(bump, "urlopen", side_effect=error):
        with pytest.raises(ValueError, match="not available on PyPI"):
            bump.release_state("0.4.0")


def test_post_is_not_retried_after_ambiguous_failure():
    with patch.object(bump, "urlopen", side_effect=URLError("connection reset")) as call:
        with pytest.raises(URLError):
            bump.github_api("POST", "/repos/x/y/issues", {"title": "x"})
    assert call.call_count == 1


def test_get_retries_transient_failure():
    with patch.object(
        bump,
        "urlopen",
        side_effect=[URLError("temporary"), response({"ok": True})],
    ) as call, patch.object(bump.time, "sleep"):
        assert bump.github_api("GET", "/repos/x/y") == (200, {"ok": True})
    assert call.call_count == 2


def test_pr_body_preserves_template_and_links_issue(tmp_path):
    target_repo(tmp_path)
    body = bump.pull_request_body(
        tmp_path,
        42,
        "0.4.0",
        {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
    )
    assert "### Work Item / Issue Reference" in body
    assert "> AB#N/A" in body
    assert "> GitHub Issue: #42" in body
    assert "`eng/versions/mssql-python-rs.version`: `0.3.0` -> `0.4.0`" in body


def test_reused_issue_title_tracks_the_latest_release():
    existing = {
        "number": 42,
        "title": "Update mssql-python-rs dependency to 0.3.0",
        "body": f"{bump.ISSUE_MARKER}\n\nold body",
    }
    with patch.object(bump, "github_paged", return_value=[existing]), patch.object(
        bump, "github_api", return_value=(200, {})
    ) as api:
        assert bump.ensure_issue(
            "0.4.0",
            {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
            "123",
        ) == 42
    assert api.call_args.args == (
        "PATCH",
        "/repos/microsoft/mssql-python/issues/42",
        {
            "title": "Update mssql-python-rs dependency to 0.4.0",
            "body": bump.issue_body(
                "0.4.0",
                {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
                "123",
            ),
        },
    )


def test_maintainer_owned_branch_is_not_modified(tmp_path):
    target_repo(tmp_path)
    with patch.object(
        bump, "git", side_effect=[
            subprocess.CompletedProcess("git", 0),
            subprocess.CompletedProcess("git", 0, stdout="base\n"),
        ]
    ), patch.object(
        bump,
        "remote_pin_versions",
        return_value={bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
    ), patch.object(
        bump, "remote_branch_state", return_value=(True, "their-sha", False)
    ), patch.object(
        bump, "github_paged", return_value=[{"number": 99}]
    ), patch.object(
        bump, "ensure_issue"
    ) as issue, patch.object(
        bump, "github_api"
    ) as api:
        assert bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
            "123",
        ) == 99
    issue.assert_not_called()
    api.assert_not_called()
    assert (tmp_path / bump.PYPI_PIN).read_text() == "0.3.0"


def test_closed_unmerged_maintainer_branch_requires_cleanup(tmp_path):
    target_repo(tmp_path)
    with patch.object(
        bump,
        "git",
        side_effect=[
            subprocess.CompletedProcess("git", 0),
            subprocess.CompletedProcess("git", 0, stdout="base\n"),
        ],
    ), patch.object(
        bump,
        "remote_pin_versions",
        return_value={bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
    ), patch.object(
        bump, "remote_branch_state", return_value=(True, "their-sha", False)
    ), patch.object(
        bump,
        "github_paged",
        side_effect=[
            [],
            [{"merged_at": None, "head": {"sha": "their-sha"}}],
        ],
    ), patch.object(
        bump, "branch_merged_into_base", return_value=False
    ), patch.object(
        bump, "ensure_issue"
    ) as issue:
        with pytest.raises(RuntimeError, match="delete or rename"):
            bump.update_pull_request(
                tmp_path,
                "0.4.0",
                {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
                "123",
            )
    issue.assert_not_called()


def test_closed_merged_maintainer_branch_is_recreated(tmp_path):
    target_repo(tmp_path)
    calls = []

    def git_call(root, *args, **kwargs):
        calls.append(args)
        if args == ("rev-parse", "FETCH_HEAD"):
            return subprocess.CompletedProcess("git", 0, stdout="base\n")
        return subprocess.CompletedProcess("git", 0, stdout="")

    with patch.object(bump, "git", side_effect=git_call), patch.object(
        bump, "remote_branch_state", return_value=(True, "their-sha", False)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
        ],
    ), patch.object(
        bump,
        "github_paged",
        side_effect=[
            [],
            [
                {
                    "merged_at": "2026-10-20T00:00:00Z",
                    "head": {"sha": "their-sha"},
                }
            ],
        ],
    ), patch.object(
        bump, "branch_merged_into_base", return_value=False
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ), patch.object(
        bump, "github_api", return_value=(201, {"number": 100})
    ):
        assert bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
            "123",
        ) == 100
    assert any(args[0] == "push" for args in calls)


def test_closed_pr_for_different_branch_sha_does_not_allow_recreation(tmp_path):
    target_repo(tmp_path)
    with patch.object(
        bump,
        "git",
        side_effect=[
            subprocess.CompletedProcess("git", 0),
            subprocess.CompletedProcess("git", 0, stdout="base\n"),
        ],
    ), patch.object(
        bump,
        "remote_pin_versions",
        return_value={bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
    ), patch.object(
        bump, "remote_branch_state", return_value=(True, "current-sha", False)
    ), patch.object(
        bump,
        "github_paged",
        side_effect=[
            [],
            [
                {
                    "merged_at": "2026-10-20T00:00:00Z",
                    "head": {"sha": "older-sha"},
                }
            ],
        ],
    ), patch.object(
        bump, "branch_merged_into_base", return_value=False
    ):
        with pytest.raises(RuntimeError, match="delete or rename"):
            bump.update_pull_request(
                tmp_path,
                "0.4.0",
                {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
                "123",
            )


def test_missing_remote_branch_is_distinguished_from_fetch_failure(tmp_path):
    missing = subprocess.CompletedProcess("git", 128, stdout="", stderr="missing")
    absent = subprocess.CompletedProcess("git", 2, stdout="", stderr="")
    with patch.object(bump, "git", side_effect=[missing, absent]):
        assert bump.remote_branch_state(tmp_path, "base") == (False, None, True)


def test_remote_branch_transport_failure_propagates(tmp_path):
    failure = subprocess.CompletedProcess("git", 128, stdout="", stderr="network down")
    with patch.object(bump, "git", side_effect=[failure, failure]):
        with pytest.raises(RuntimeError, match="network down"):
            bump.remote_branch_state(tmp_path, "base")


def test_stale_event_does_not_replace_newer_pending_pins(tmp_path):
    target_repo(tmp_path)
    with patch.object(
        bump,
        "git",
        side_effect=[
            subprocess.CompletedProcess("git", 0),
            subprocess.CompletedProcess("git", 0, stdout="base\n"),
        ],
    ), patch.object(
        bump, "remote_branch_state", return_value=(True, "pending-sha", True)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.5.0", bump.NUGET_PIN: "0.5.0"},
        ],
    ), patch.object(
        bump, "github_paged", return_value=[{"number": 99}]
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ) as issue, patch.object(
        bump, "github_api", return_value=(200, {})
    ) as api:
        assert bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {
                bump.PYPI_PIN: ("0.3.0", "0.4.0"),
                bump.NUGET_PIN: ("0.3.0", "0.4.0"),
            },
            "123",
        ) == 99
    assert issue.call_args.args[0] == "0.5.0"
    assert api.call_args.args[2]["title"] == (
        "CHORE: Update mssql-python-rs dependency to 0.5.0"
    )


def test_matching_pending_pins_do_not_rebase_for_unrelated_main_changes(tmp_path):
    target_repo(tmp_path)
    calls = []

    def git_call(root, *args, **kwargs):
        calls.append(args)
        if args == ("rev-parse", "FETCH_HEAD"):
            return subprocess.CompletedProcess("git", 0, stdout="base\n")
        return subprocess.CompletedProcess("git", 0, stdout="")

    with patch.object(bump, "git", side_effect=git_call), patch.object(
        bump, "remote_branch_state", return_value=(True, "pending-sha", True)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.4.0", bump.NUGET_PIN: "0.3.0"},
        ],
    ), patch.object(
        bump, "github_paged", return_value=[{"number": 99}]
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ), patch.object(
        bump, "github_api", return_value=(200, {})
    ):
        assert bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
            "123",
        ) == 99
    assert not any(args[0] in {"switch", "commit", "push"} for args in calls)


def test_matching_branch_without_pull_request_creates_missing_pr(tmp_path):
    target_repo(tmp_path)
    calls = []

    def git_call(root, *args, **kwargs):
        calls.append(args)
        if args == ("rev-parse", "FETCH_HEAD"):
            return subprocess.CompletedProcess("git", 0, stdout="base\n")
        return subprocess.CompletedProcess("git", 0, stdout="")

    with patch.object(bump, "git", side_effect=git_call), patch.object(
        bump, "remote_branch_state", return_value=(True, "pending-sha", True)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.4.0", bump.NUGET_PIN: "0.3.0"},
        ],
    ), patch.object(
        bump, "github_paged", return_value=[]
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ), patch.object(
        bump, "github_api", return_value=(201, {"number": 100})
    ) as api:
        assert bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
            "123",
        ) == 100
    assert not any(args[0] in {"switch", "commit", "push"} for args in calls)
    assert api.call_args.args[0:2] == (
        "POST",
        "/repos/microsoft/mssql-python/pulls",
    )
    assert api.call_args.args[2]["draft"] is True


def test_matching_branch_repairs_pr_metadata_after_failed_patch(tmp_path):
    target_repo(tmp_path)
    calls = []

    def git_call(root, *args, **kwargs):
        calls.append(args)
        if args == ("rev-parse", "FETCH_HEAD"):
            return subprocess.CompletedProcess("git", 0, stdout="base\n")
        return subprocess.CompletedProcess("git", 0, stdout="")

    with patch.object(bump, "git", side_effect=git_call), patch.object(
        bump, "remote_branch_state", return_value=(True, "pending-sha", True)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.5.0", bump.NUGET_PIN: "0.3.0"},
        ],
    ), patch.object(
        bump, "github_paged", return_value=[{"number": 99}]
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ), patch.object(
        bump, "github_api", return_value=(200, {})
    ) as api:
        assert bump.update_pull_request(
            tmp_path,
            "0.5.0",
            {bump.PYPI_PIN: ("0.3.0", "0.5.0")},
            "123",
        ) == 99
    assert not any(args[0] in {"switch", "commit", "push"} for args in calls)
    assert api.call_args.args[0:2] == (
        "PATCH",
        "/repos/microsoft/mssql-python/pulls/99",
    )
    assert api.call_args.args[2]["title"] == (
        "CHORE: Update mssql-python-rs dependency to 0.5.0"
    )


def test_older_partial_match_keeps_newer_canonical_metadata(tmp_path):
    target_repo(tmp_path)
    effective = {
        bump.PYPI_PIN: ("0.3.0", "0.5.0"),
        bump.NUGET_PIN: ("0.3.0", "0.4.0"),
    }
    canonical_title = "CHORE: Update mssql-python-rs dependency to 0.5.0"
    canonical_body = bump.pull_request_body(tmp_path, 42, "0.5.0", effective)

    with patch.object(
        bump,
        "git",
        side_effect=[
            subprocess.CompletedProcess("git", 0),
            subprocess.CompletedProcess("git", 0, stdout="base\n"),
        ],
    ), patch.object(
        bump, "remote_branch_state", return_value=(True, "pending-sha", True)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.5.0", bump.NUGET_PIN: "0.4.0"},
        ],
    ), patch.object(
        bump,
        "github_paged",
        return_value=[
            {"number": 99, "title": canonical_title, "body": canonical_body}
        ],
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ) as issue, patch.object(
        bump, "github_api"
    ) as api:
        assert bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {
                bump.PYPI_PIN: ("0.3.0", "0.4.0"),
                bump.NUGET_PIN: ("0.3.0", "0.4.0"),
            },
            "older-build",
        ) == 99
    assert issue.call_args.args[0] == "0.5.0"
    api.assert_not_called()


def test_changed_pending_pins_use_inspected_sha_as_push_lease(tmp_path):
    target_repo(tmp_path)
    calls = []

    def git_call(root, *args, **kwargs):
        calls.append(args)
        if args == ("rev-parse", "FETCH_HEAD"):
            return subprocess.CompletedProcess("git", 0, stdout="base\n")
        return subprocess.CompletedProcess("git", 0, stdout="")

    with patch.object(bump, "git", side_effect=git_call), patch.object(
        bump, "remote_branch_state", return_value=(True, "pending-sha", True)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.4.0", bump.NUGET_PIN: "0.3.0"},
        ],
    ), patch.object(
        bump, "github_paged", return_value=[{"number": 99}]
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ), patch.object(
        bump, "github_api", return_value=(200, {})
    ):
        bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {
                bump.PYPI_PIN: ("0.3.0", "0.4.0"),
                bump.NUGET_PIN: ("0.3.0", "0.4.0"),
            },
            "123",
        )
    assert (
        "push",
        f"--force-with-lease=refs/heads/{bump.BRANCH}:pending-sha",
        "origin",
        f"{bump.BRANCH}:refs/heads/{bump.BRANCH}",
    ) in calls


def test_pypi_only_release_preserves_pending_clean_nuget_upgrade(tmp_path):
    target_repo(tmp_path)
    calls = []

    def git_call(root, *args, **kwargs):
        calls.append(args)
        if args == ("rev-parse", "FETCH_HEAD"):
            return subprocess.CompletedProcess("git", 0, stdout="base\n")
        return subprocess.CompletedProcess("git", 0, stdout="")

    with patch.object(bump, "git", side_effect=git_call), patch.object(
        bump, "remote_branch_state", return_value=(True, "pending-sha", True)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.3.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.4.0", bump.NUGET_PIN: "0.4.0"},
        ],
    ), patch.object(
        bump, "github_paged", return_value=[{"number": 99}]
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ) as issue, patch.object(
        bump, "github_api", return_value=(200, {})
    ):
        bump.update_pull_request(
            tmp_path,
            "0.5.0",
            {bump.PYPI_PIN: ("0.3.0", "0.5.0")},
            "123",
        )
    effective = issue.call_args.args[1]
    assert effective == {
        bump.PYPI_PIN: ("0.3.0", "0.5.0"),
        bump.NUGET_PIN: ("0.3.0", "0.4.0"),
    }
    assert (tmp_path / bump.PYPI_PIN).read_text() == "0.5.0"
    assert (tmp_path / bump.NUGET_PIN).read_text() == "0.4.0"


def test_delayed_nuget_release_advances_behind_newer_pending_pypi(tmp_path):
    target_repo(tmp_path, "0.5.0", "0.3.0")
    calls = []

    def git_call(root, *args, **kwargs):
        calls.append(args)
        if args == ("rev-parse", "FETCH_HEAD"):
            return subprocess.CompletedProcess("git", 0, stdout="base\n")
        return subprocess.CompletedProcess("git", 0, stdout="")

    with patch.object(bump, "git", side_effect=git_call), patch.object(
        bump, "remote_branch_state", return_value=(True, "pending-sha", True)
    ), patch.object(
        bump,
        "remote_pin_versions",
        side_effect=[
            {bump.PYPI_PIN: "0.5.0", bump.NUGET_PIN: "0.3.0"},
            {bump.PYPI_PIN: "0.5.0", bump.NUGET_PIN: "0.3.0"},
        ],
    ), patch.object(
        bump, "github_paged", return_value=[{"number": 99}]
    ), patch.object(
        bump, "ensure_issue", return_value=42
    ) as issue, patch.object(
        bump, "github_api", return_value=(200, {})
    ):
        bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {bump.NUGET_PIN: ("0.3.0", "0.4.0")},
            "123",
        )
    assert issue.call_args.args[1] == {
        bump.NUGET_PIN: ("0.3.0", "0.4.0"),
    }
    assert (tmp_path / bump.PYPI_PIN).read_text() == "0.5.0"
    assert (tmp_path / bump.NUGET_PIN).read_text() == "0.4.0"
    assert (
        "push",
        f"--force-with-lease=refs/heads/{bump.BRANCH}:pending-sha",
        "origin",
        f"{bump.BRANCH}:refs/heads/{bump.BRANCH}",
    ) in calls


def test_main_pin_change_during_planning_aborts_before_writing(tmp_path):
    target_repo(tmp_path)
    with patch.object(
        bump,
        "git",
        side_effect=[
            subprocess.CompletedProcess("git", 0),
            subprocess.CompletedProcess("git", 0, stdout="new-base\n"),
        ],
    ), patch.object(
        bump,
        "remote_pin_versions",
        return_value={bump.PYPI_PIN: "0.5.0", bump.NUGET_PIN: "0.5.0"},
    ), patch.object(bump, "remote_branch_state") as branch, patch.object(
        bump, "github_paged"
    ) as pulls, patch.object(bump, "ensure_issue") as issue:
        with pytest.raises(RuntimeError, match="changed dependency pins"):
            bump.update_pull_request(
                tmp_path,
                "0.4.0",
                {
                    bump.PYPI_PIN: ("0.3.0", "0.4.0"),
                    bump.NUGET_PIN: ("0.3.0", "0.4.0"),
                },
                "123",
            )
    branch.assert_not_called()
    pulls.assert_not_called()
    issue.assert_not_called()


def test_worker_pipeline_is_manual_and_uses_persisted_github_credential():
    pipeline = yaml.safe_load(PIPELINE.read_text(encoding="utf-8"))
    assert pipeline["trigger"] == "none"
    assert pipeline["pr"] == "none"
    steps = worker_steps(pipeline)
    checkout = next(step for step in steps if step.get("checkout") == "self")
    assert checkout["persistCredentials"] is True
    worker = next(
        step for step in steps
        if step.get("displayName") == "Open mssql-python dependency update pull request"
    )
    assert worker["env"]["RELEASED_VERSION"] == "${{ parameters.releasedVersion }}"
    assert worker["env"]["MSSQL_PYTHON_REPOSITORY"] == "microsoft/mssql-python"
    assert worker["env"]["DEFAULT_BRANCH"] == "main"


@pytest.mark.skipif(BASH is None, reason="Bash is required")
def test_worker_token_extraction_finds_the_repo_scoped_key(tmp_path):
    pipeline = yaml.safe_load(PIPELINE.read_text(encoding="utf-8"))
    extract = next(
        step
        for step in worker_steps(pipeline)
        if step.get("displayName") == "Read GitHub token from checkout credentials"
    )

    token = "ghs_worker_token"
    encoded = base64.b64encode(f"x-access-token:{token}".encode()).decode()
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(
        [
            "git",
            "config",
            "--local",
            "http.https://github.com/microsoft/mssql-rs.extraheader",
            f"AUTHORIZATION: basic {encoded}",
        ],
        cwd=repo,
        check=True,
    )
    result = subprocess.run(
        [BASH, "-c", shell_script(extract)],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
    assert (
        f"##vso[task.setvariable variable=ghToken;issecret=true]{token}"
        in result.stdout
    )
    assert (
        "Extracted GitHub token from "
        "http.https://github.com/microsoft/mssql-rs.extraheader."
        in result.stdout
    )


def test_worker_serializes_github_writers_with_an_environment_lock():
    pipeline = yaml.safe_load(PIPELINE.read_text(encoding="utf-8"))
    assert pipeline["lockBehavior"] == "sequential"
    assert pipeline["stages"][0]["condition"] == (
        "and(succeeded(), eq(variables['Build.SourceBranch'], 'refs/heads/main'))"
    )
    deployment = pipeline["stages"][0]["jobs"][0]
    assert deployment["deployment"] == "UpdateMssqlPython"
    assert deployment["environment"] == "mssql-python-release-bump-writer"


def test_pypi_release_queues_worker_only_after_real_publish():
    text = PYPI_RELEASE.read_text(encoding="utf-8")
    assert "Bump mssql-python Release Version" in text
    assert text.index("ESRP Release mssql-python-rs wheels to PyPI") < text.index(
        "Queue mssql-python dependency update"
    )
    assert "templateParameters" in text
    assert "refName = 'refs/heads/main'" in text
    assert 'releasedVersion = "$(releaseVersion)"' in text
    assert "publishToPyPI" in text

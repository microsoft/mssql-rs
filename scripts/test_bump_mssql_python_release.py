# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import importlib.util
import io
import json
import os
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


@pytest.mark.parametrize(
    ("current_python", "current_nuget", "available", "expected"),
    [
        ("0.3.0", "0.3.0", True, {bump.PYPI_PIN, bump.NUGET_PIN}),
        ("0.3.0", "0.3.0", False, {bump.PYPI_PIN}),
        ("0.4.0", "0.3.0", True, {bump.NUGET_PIN}),
        ("0.4.0", "0.3.0", False, set()),
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


def test_maintainer_owned_branch_is_not_modified(tmp_path):
    target_repo(tmp_path)
    with patch.object(
        bump, "git", side_effect=[
            subprocess.CompletedProcess("git", 0),
            subprocess.CompletedProcess("git", 0, stdout="base\n"),
        ]
    ), patch.object(
        bump, "remote_branch_state", return_value=(True, "tree", False)
    ), patch.object(
        bump, "github_paged", return_value=[{"number": 99}]
    ), patch.object(bump, "ensure_issue") as issue:
        assert bump.update_pull_request(
            tmp_path,
            "0.4.0",
            {bump.PYPI_PIN: ("0.3.0", "0.4.0")},
            "123",
        ) == 99
    issue.assert_not_called()
    assert (tmp_path / bump.PYPI_PIN).read_text() == "0.3.0"


def test_worker_pipeline_is_manual_and_uses_persisted_github_credential():
    pipeline = yaml.safe_load(PIPELINE.read_text(encoding="utf-8"))
    assert pipeline["trigger"] == "none"
    assert pipeline["pr"] == "none"
    checkout = next(step for step in pipeline["steps"] if step.get("checkout") == "self")
    assert checkout["persistCredentials"] is True
    worker = next(
        step for step in pipeline["steps"]
        if step.get("displayName") == "Open mssql-python dependency update pull request"
    )
    assert worker["env"]["RELEASED_VERSION"] == "${{ parameters.releasedVersion }}"
    assert worker["env"]["MSSQL_PYTHON_REPOSITORY"] == "microsoft/mssql-python"
    assert worker["env"]["DEFAULT_BRANCH"] == "main"


def test_pypi_release_queues_worker_only_after_real_publish():
    text = PYPI_RELEASE.read_text(encoding="utf-8")
    assert "Bump mssql-python Release Version" in text
    assert text.index("ESRP Release mssql-python-rs wheels to PyPI") < text.index(
        "Queue mssql-python dependency update"
    )
    assert "templateParameters" in text
    assert 'releasedVersion = "$(releaseVersion)"' in text
    assert "publishToPyPI" in text

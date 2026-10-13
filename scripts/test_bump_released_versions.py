# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import importlib.util
import io
import json
import re
import subprocess
from collections import Counter
from datetime import datetime, timedelta, timezone
from pathlib import Path
from urllib.error import HTTPError, URLError
from unittest.mock import Mock, patch

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "bump", ROOT / "scripts" / "bump-released-versions.py"
)
bump = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bump)

WORKFLOW = ROOT / ".github" / "workflows" / "bump-released-versions.yml"


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
    current = {"mssql-tds": "0.2.0", "mssql-mock-tds": "0.2.0"}
    published = {"mssql-tds": {"0.2.0"}, "mssql-mock-tds": set()}
    with patch.object(bump, "cargo_versions", return_value=current):
        assert bump.planned_bumps(tmp_path, published) == {
            "mssql-tds": ("0.2.0", "0.3.0"),
            "mssql-mock-tds": ("0.2.0", "0.3.0"),
        }


def test_drifted_core_and_mock_versions_fail(tmp_path):
    current = {"mssql-tds": "0.2.0", "mssql-mock-tds": "0.1.0"}
    published = {"mssql-tds": {"0.2.0"}, "mssql-mock-tds": set()}
    with patch.object(bump, "cargo_versions", return_value=current):
        with pytest.raises(ValueError, match="must match for a release"):
            bump.planned_bumps(tmp_path, published)


def test_aligned_mock_target_already_published_fails(tmp_path):
    current = {"mssql-tds": "0.2.0", "mssql-mock-tds": "0.2.0"}
    published = {"mssql-tds": {"0.2.0"}, "mssql-mock-tds": {"0.3.0"}}
    with patch.object(bump, "cargo_versions", return_value=current):
        with pytest.raises(ValueError, match=r"mssql-mock-tds 0\.3\.0 is already published"):
            bump.planned_bumps(tmp_path, published)


def test_mock_ahead_of_core_target_fails(tmp_path):
    current = {"mssql-tds": "0.2.0", "mssql-mock-tds": "0.4.0"}
    published = {"mssql-tds": {"0.2.0"}, "mssql-mock-tds": set()}
    with patch.object(bump, "cargo_versions", return_value=current):
        with pytest.raises(ValueError, match="must match for a release"):
            bump.planned_bumps(tmp_path, published)


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


@pytest.mark.parametrize("version", ["0.2.0-rc.1", "0.2.0-dev.20240101"])
def test_next_minor_rejects_prerelease_versions(version):
    with pytest.raises(ValueError, match="not a valid three-part release version"):
        bump.next_minor(version)


def test_metadata_uses_cargo_json(tmp_path):
    packages = [{"name": crate, "version": "0.1.7"} for crate in bump.CRATES]
    with patch.object(bump.subprocess, "run", return_value=subprocess.CompletedProcess(
        "cargo", 0, stdout=json.dumps({"packages": packages})
    )) as cargo:
        assert bump.cargo_versions(tmp_path) == dict.fromkeys(bump.CRATES, "0.1.7")
    cargo.assert_called_once_with(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=tmp_path, check=True, stdout=subprocess.PIPE, text=True,
    )


def test_missing_crate_in_cargo_metadata_fails(tmp_path):
    packages = [{"name": "mssql-tds", "version": "0.1.7"}]
    with patch.object(bump.subprocess, "run", return_value=subprocess.CompletedProcess(
        "cargo", 0, stdout=json.dumps({"packages": packages})
    )):
        with pytest.raises(ValueError, match="mssql-mock-tds not found"):
            bump.cargo_versions(tmp_path)


# --- registries -------------------------------------------------------------


def test_registry_response_includes_yanked_and_old_versions():
    response = io.BytesIO(json.dumps({
        "versions": [{"num": "0.2.0", "yanked": False}, {"num": "0.1.0", "yanked": True}]
    }).encode())
    with patch.object(bump, "urlopen", return_value=response) as request:
        assert bump.published_versions("mssql-tds") == {"0.1.0", "0.2.0"}
    assert request.call_args.kwargs["timeout"] == 30
    assert request.call_args.args[0].full_url == "https://crates.io/api/v1/crates/mssql-tds"
    assert request.call_args.args[0].get_header("User-agent") == (
        "microsoft/mssql-rs version check (https://github.com/microsoft/mssql-rs)"
    )


def test_pypi_releases_are_published_versions():
    response = io.BytesIO(json.dumps({"releases": {"0.2.0": [], "0.3.0": []}}).encode())
    with patch.object(bump, "urlopen", return_value=response) as request:
        assert bump.pypi_published_versions(bump.PYTHON_DIST) == {"0.2.0", "0.3.0"}
    assert request.call_args.args[0].full_url == (
        f"https://pypi.org/pypi/{bump.PYTHON_DIST}/json"
    )


@pytest.mark.parametrize(
    "lookup", [bump.published_versions, bump.pypi_published_versions]
)
@pytest.mark.parametrize("code", [404, 403, 429, 500])
def test_registry_http_errors(lookup, code):
    error = HTTPError("https://registry.example", code, "test", {}, None)
    with patch.object(bump, "urlopen", side_effect=error):
        if code == 404:
            assert lookup("mssql-tds") == set()
        else:
            with pytest.raises(HTTPError):
                lookup("mssql-tds")


@pytest.mark.parametrize("payload", [b"not json", b"{}", b'{"versions": []}'])
def test_bad_registry_data_fails(payload):
    with patch.object(bump, "urlopen", return_value=io.BytesIO(payload)):
        with pytest.raises((ValueError, KeyError)):
            bump.published_versions("mssql-tds")


@pytest.mark.parametrize("payload", [b"not json", b"{}", b'{"releases": {}}'])
def test_bad_pypi_data_fails(payload):
    with patch.object(bump, "urlopen", return_value=io.BytesIO(payload)):
        with pytest.raises((ValueError, KeyError)):
            bump.pypi_published_versions(bump.PYTHON_DIST)


def test_registry_retries_transient_failures():
    payload = json.dumps({"versions": [{"num": "0.2.0"}]}).encode()
    error = HTTPError("https://crates.io", 503, "temporary outage", {}, None)
    with patch.object(bump, "urlopen", side_effect=[error, io.BytesIO(payload)]) as request:
        with patch.object(bump.time, "sleep") as sleep:
            assert bump.published_versions("mssql-tds") == {"0.2.0"}
    assert request.call_count == 2
    sleep.assert_called_once_with(1)


def test_registry_timeout_fails():
    with patch.object(bump, "urlopen", side_effect=URLError("timed out")):
        with pytest.raises(URLError):
            bump.published_versions("mssql-tds")


# --- manifest edits ---------------------------------------------------------


@pytest.fixture
def manifests(tmp_path):
    (tmp_path / "mssql-tds").mkdir()
    (tmp_path / "mssql-tds" / "Cargo.toml").write_text(
        '[package]\nname = "mssql-tds"\nversion = "0.2.0"\nedition = "2024"\n\n'
        '[dependencies]\ntokio = { version = "1.48.0" }\n',
        encoding="utf-8",
    )
    (tmp_path / "mssql-mock-tds").mkdir()
    (tmp_path / "mssql-mock-tds" / "Cargo.toml").write_text(
        '[package]\nname = "mssql-mock-tds"\nversion = "0.2.0"\nedition = "2024"\n\n'
        '[dependencies]\n'
        'mssql-tds = { path = "../mssql-tds", version = "0.2.0", default-features = false }\n'
        'bytes = "1.11.0"\n',
        encoding="utf-8",
    )
    (tmp_path / bump.PYPROJECT).parent.mkdir()
    (tmp_path / bump.PYPROJECT).write_text(
        '[build-system]\nrequires = ["maturin"]\n\n'
        '[project]\nname = "mssql-python-rs"\nversion = "0.3.0"\n'
        'requires-python = ">=3.10"\n',
        encoding="utf-8",
    )
    return tmp_path


def test_reads_versions_from_the_right_table(manifests):
    assert bump.read_table_version(manifests / "mssql-tds" / "Cargo.toml", "package") == "0.2.0"
    assert bump.read_table_version(manifests / bump.PYPROJECT, "project") == "0.3.0"


def test_crate_bump_rewrites_manifests_and_dependency(manifests):
    changes = dict.fromkeys(bump.CRATES, ("0.2.0", "0.3.0"))
    assert bump.apply_crate_bumps(manifests, changes) == [
        "mssql-mock-tds/Cargo.toml", "mssql-tds/Cargo.toml"
    ]
    core = (manifests / "mssql-tds" / "Cargo.toml").read_text(encoding="utf-8")
    mock = (manifests / "mssql-mock-tds" / "Cargo.toml").read_text(encoding="utf-8")
    assert '[package]\nname = "mssql-tds"\nversion = "0.3.0"' in core
    # A version requirement in [dependencies] must not be mistaken for the package version.
    assert 'tokio = { version = "1.48.0" }' in core
    assert '[package]\nname = "mssql-mock-tds"\nversion = "0.3.0"' in mock
    assert (
        'mssql-tds = { path = "../mssql-tds", version = "0.3.0", default-features = false }'
        in mock
    )


def test_core_only_bump_still_touches_the_mock_manifest(manifests):
    paths = bump.apply_crate_bumps(manifests, {"mssql-tds": ("0.2.0", "0.3.0")})
    mock = (manifests / "mssql-mock-tds" / "Cargo.toml").read_text(encoding="utf-8")
    assert "mssql-mock-tds/Cargo.toml" in paths
    assert '[package]\nname = "mssql-mock-tds"\nversion = "0.2.0"' in mock
    assert 'version = "0.3.0", default-features = false' in mock


def test_python_bump_rewrites_pyproject_only(manifests):
    paths = bump.apply_python_bump(manifests, {bump.PYTHON_DIST: ("0.3.0", "0.4.0")})
    text = (manifests / bump.PYPROJECT).read_text(encoding="utf-8")
    assert paths == ["mssql-py-core/pyproject.toml"]
    assert '[project]\nname = "mssql-python-rs"\nversion = "0.4.0"' in text
    assert '[build-system]\nrequires = ["maturin"]' in text


def test_missing_version_field_fails(tmp_path):
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text('[package]\nname = "x"\n', encoding="utf-8")
    with pytest.raises(ValueError, match="no \\[package\\] version field"):
        bump.write_table_version(manifest, "package", "0.3.0")


def test_missing_mock_dependency_fails(tmp_path):
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text('[dependencies]\nbytes = "1.11.0"\n', encoding="utf-8")
    with pytest.raises(ValueError, match="no versioned mssql-tds path dependency"):
        bump.write_mock_tds_dependency(manifest, "0.3.0")


# --- python track planning --------------------------------------------------


def test_python_bump_planned_when_current_version_is_published(manifests):
    assert bump.planned_python_bump(manifests, {"0.2.0", "0.3.0"}) == {
        bump.PYTHON_DIST: ("0.3.0", "0.4.0")
    }


def test_python_bump_skipped_when_current_version_is_unpublished(manifests):
    assert bump.planned_python_bump(manifests, {"0.2.0"}) == {}


def test_python_bump_fails_when_target_is_published(manifests):
    with pytest.raises(ValueError, match=r"0\.4\.0 is already published"):
        bump.planned_python_bump(manifests, {"0.3.0", "0.4.0"})


# --- duplicate pull request detection ---------------------------------------


@pytest.fixture
def workflow_environment(tmp_path, monkeypatch):
    monkeypatch.setattr(bump, "__file__", str(tmp_path / "scripts" / "bump.py"))
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    monkeypatch.setenv("DEFAULT_BRANCH", "main")

    def unexpected_command(args, **kwargs):
        if args[:3] == ["gh", "pr", "list"]:
            return subprocess.CompletedProcess(args, 0, stdout="[]")
        raise AssertionError("Unexpected external command")

    monkeypatch.setattr(bump.subprocess, "run", Mock(side_effect=unexpected_command))
    return tmp_path


def pr_list(*pulls):
    return subprocess.CompletedProcess("gh", 0, stdout=json.dumps(list(pulls)))


def test_has_open_bump_pr(workflow_environment):
    changes = {"mssql-tds": ("0.1.7", "0.2.0")}
    with patch.object(bump.subprocess, "run", return_value=pr_list(
        {"number": 584, "title": "Fix mssql-tds docs", "body": "Mentions mssql-tds only",
         "headRefName": "docs"},
        {"number": 597, "title": "Bump mssql-tds", "body": "- `mssql-tds`: `0.1.7` -> `0.2.0`",
         "headRefName": "manual-bump"},
    )) as gh:
        assert bump.has_open_bump_pr(workflow_environment, changes, bump.CRATE_BRANCH)
    gh.assert_called_once_with(
        [
            "gh", "pr", "list", "--repo", "microsoft/mssql-rs",
            "--base", "main", "--state", "open",
            "--json", "number,title,body,headRefName", "--limit", "100",
        ],
        cwd=workflow_environment, check=True, stdout=subprocess.PIPE, text=True,
    )


def test_our_own_pr_is_not_treated_as_a_duplicate(workflow_environment):
    with patch.object(bump.subprocess, "run", return_value=pr_list(
        {"number": 600, "title": "Bump released TDS crates to 0.2.0",
         "body": "- `mssql-tds`: `0.1.7` -> `0.2.0`", "headRefName": bump.CRATE_BRANCH},
    )):
        assert not bump.has_open_bump_pr(
            workflow_environment, {"mssql-tds": ("0.1.7", "0.2.0")}, bump.CRATE_BRANCH
        )


def test_open_bump_pr_requires_every_selected_crate(workflow_environment):
    changes = dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))
    with patch.object(bump.subprocess, "run", return_value=pr_list(
        {"number": 600, "title": "Bump mssql-tds", "body": "- `mssql-tds`: `0.1.7` -> `0.2.0`",
         "headRefName": "manual-bump"},
    )):
        assert not bump.has_open_bump_pr(workflow_environment, changes, bump.CRATE_BRANCH)


def test_open_bump_pr_requires_exact_transition(workflow_environment):
    with patch.object(bump.subprocess, "run", return_value=pr_list(
        {"number": 601, "title": "Bump mssql-tds to 0.2.0",
         "body": "- `mssql-tds`: `0.1.6` -> `0.2.0`", "headRefName": "manual-bump"},
    )):
        assert not bump.has_open_bump_pr(
            workflow_environment, {"mssql-tds": ("0.1.7", "0.2.0")}, bump.CRATE_BRANCH
        )


def test_open_bump_pr_requires_summary_line_format(workflow_environment):
    with patch.object(bump.subprocess, "run", return_value=pr_list(
        {"number": 602, "title": "Compatibility notes",
         "body": "Compatibility notes for mssql-tds 0.1.7 and 0.2.0", "headRefName": "notes"},
    )):
        assert not bump.has_open_bump_pr(
            workflow_environment, {"mssql-tds": ("0.1.7", "0.2.0")}, bump.CRATE_BRANCH
        )


# --- branch and pull request creation ---------------------------------------


def record_git_and_gh(existing_pr=None):
    calls = []

    def run(args, **kwargs):
        calls.append(args)
        if args[0] == "git":
            return subprocess.CompletedProcess(args, 0, stdout="")
        if args[:3] == ["gh", "pr", "list"]:
            pulls = [{"number": existing_pr}] if existing_pr else []
            return subprocess.CompletedProcess(args, 0, stdout=json.dumps(pulls))
        return subprocess.CompletedProcess(
            args, 0, stdout="https://github.com/microsoft/mssql-rs/pull/700\n"
        )

    return calls, run


def test_open_bump_pr_branches_before_editing_and_creates(workflow_environment):
    calls, run = record_git_and_gh()
    edited_on = []
    with patch.object(bump.subprocess, "run", side_effect=run):
        bump.open_bump_pr(
            workflow_environment, bump.CRATE_BRANCH, "abc123",
            "Bump released TDS crates to 0.3.0", "body",
            lambda root: (edited_on.append(list(calls)), ["mssql-tds/Cargo.toml"])[1],
        )
    # Edits must be applied only after the branch exists, so a previous track's
    # commit on another branch cannot leak into this one.
    assert edited_on[0] == [["git", "switch", "--force-create", bump.CRATE_BRANCH, "abc123"]]
    assert calls[0] == ["git", "switch", "--force-create", bump.CRATE_BRANCH, "abc123"]
    assert calls[1] == ["git", "add", "--", "mssql-tds/Cargo.toml"]
    assert calls[2] == ["git", "commit", "--message", "Bump released TDS crates to 0.3.0"]
    assert calls[3] == [
        "git", "push", "--force", "origin",
        f"{bump.CRATE_BRANCH}:refs/heads/{bump.CRATE_BRANCH}",
    ]
    assert calls[-1][:3] == ["gh", "pr", "create"]
    assert "--base" in calls[-1] and calls[-1][calls[-1].index("--base") + 1] == "main"


def test_open_bump_pr_updates_existing_pull_request(workflow_environment):
    calls, run = record_git_and_gh(existing_pr=694)
    with patch.object(bump.subprocess, "run", side_effect=run):
        assert bump.open_bump_pr(
            workflow_environment, bump.PYTHON_BRANCH, "abc123",
            "Bump mssql-python-rs to 0.4.0", "body",
            lambda root: ["mssql-py-core/pyproject.toml"],
        ) == 694
    assert calls[-1][:3] == ["gh", "pr", "edit"]
    assert calls[-1][3] == "694"
    assert not any(call[:3] == ["gh", "pr", "create"] for call in calls)


def test_pull_request_body_links_an_issue_for_the_linked_issue_check():
    body = bump.pull_request_body("- `mssql-tds`: `0.2.0` -> `0.3.0`", 693, "Extra note.")
    assert "Fixes #693" in body
    assert "## Description" in body
    assert "## Related Issues" in body
    assert "## Checklist" in body
    assert "Extra note." in body
    # PR Linked Issue Check rejects a pull request whose body has no `#<number>`
    # issue reference or ADO work item link.
    assert re.search(r"#\d+", body)


# --- tracking issues --------------------------------------------------------


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
            assert args[3] == bump.CRATE_LABEL
            assert args[args.index("--repo") + 1] == "microsoft/mssql-rs"
            assert "--force" in args
            labels_created.append(args[3])
            return subprocess.CompletedProcess(args, 0, stdout="")
        endpoint = "repos/microsoft/mssql-rs/issues"
        if "--method" not in args:
            assert args[:3] == ["gh", "api", "graphql"]
            assert args[5:] == [
                "-f", "owner=microsoft", "-f", "name=mssql-rs",
                "-f", f"label={bump.CRATE_LABEL}", "--paginate", "--slurp",
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
            assert payload["labels"] == [bump.CRATE_LABEL]
            stored.append({"number": 123, **payload})
        else:
            assert method == "PATCH"
            assert "labels" not in payload
            stored[0].update(payload)
        return subprocess.CompletedProcess(args, 0, stdout=json.dumps(stored[0]))

    first = {"mssql-tds": ("0.1.7", "0.2.0")}
    both = dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))
    title = "Bump released crates to the next minor version"
    summary = "- `mssql-tds`: `0.1.7` -> `0.2.0`"
    with patch.object(bump.subprocess, "run", side_effect=github):
        for _ in range(2):
            assert bump.ensure_bump_issue(
                bump.CRATE_LABEL, bump.CRATE_MARKER, title,
                bump.crate_issue_body(summary, first),
            ) == 123
        assert writes == ["POST"]
        assert bump.ensure_bump_issue(
            bump.CRATE_LABEL, bump.CRATE_MARKER, title,
            bump.crate_issue_body("Both bumps", both),
        ) == 123
    assert writes == ["POST", "PATCH"]
    assert labels_created == [bump.CRATE_LABEL]
    assert len(stored) == 1
    body = stored[0]["body"]
    assert body.startswith(bump.CRATE_MARKER)
    assert "Both bumps" in body
    assert summary not in body
    assert "mssql-tds/Cargo.toml" in body
    assert 'version = "0.2.0"' in body
    assert 'mssql-tds = { path = "../mssql-tds", version = "0.2.0", default-features = false }' in body
    assert "put maintainer notes in issue comments" in body
    template = yaml.safe_load(
        (ROOT / ".github" / "ISSUE_TEMPLATE" / "feature_request.yml").read_text()
    )
    for field in template["body"]:
        if field["type"] != "markdown":
            assert f"### {field['attributes']['label']}\n" in body


def test_python_issue_body_matches_the_issue_template():
    body = bump.python_issue_body(
        bump.summary_line(bump.PYTHON_DIST, "0.3.0", "0.4.0"),
        {bump.PYTHON_DIST: ("0.3.0", "0.4.0")},
    )
    assert body.startswith(bump.PYTHON_MARKER)
    assert "mssql-py-core/pyproject.toml" in body
    assert 'version = "0.4.0"' in body
    assert "`mssql-py-core` Rust crate is unchanged" in body
    template = yaml.safe_load(
        (ROOT / ".github" / "ISSUE_TEMPLATE" / "feature_request.yml").read_text()
    )
    for field in template["body"]:
        if field["type"] != "markdown":
            assert f"### {field['attributes']['label']}\n" in body


def test_crate_and_python_tracks_use_distinct_issue_identities():
    assert bump.CRATE_LABEL != bump.PYTHON_LABEL
    assert bump.CRATE_MARKER != bump.PYTHON_MARKER
    assert bump.CRATE_BRANCH != bump.PYTHON_BRANCH


@pytest.mark.parametrize("operation", ["list", "label", "create", "update"])
def test_issue_api_errors_propagate(monkeypatch, operation):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    failure = subprocess.CalledProcessError(1, "gh")
    pages = [[{"number": 123, "body": bump.CRATE_MARKER}]] if operation == "update" else [[]]
    listed = subprocess.CompletedProcess("gh", 0, stdout=issue_pages(*pages))
    results = {
        "list": [failure],
        "label": [listed, failure],
        "create": [listed, subprocess.CompletedProcess("gh", 0, stdout=""), failure],
        "update": [listed, failure],
    }[operation]
    with patch.object(bump.subprocess, "run", side_effect=results) as github:
        with pytest.raises(subprocess.CalledProcessError):
            bump.ensure_bump_issue(bump.CRATE_LABEL, bump.CRATE_MARKER, "Bump", "body")
    assert github.call_count == len(results)


def test_duplicate_tracking_issues_fail_without_writing(monkeypatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    pages = [[{"number": number, "body": bump.CRATE_MARKER} for number in (123, 456)]]
    with patch.object(bump.subprocess, "run", return_value=subprocess.CompletedProcess(
        "gh", 0, stdout=issue_pages(*pages)
    )) as github:
        with pytest.raises(ValueError, match="Multiple open"):
            bump.ensure_bump_issue(bump.CRATE_LABEL, bump.CRATE_MARKER, "Bump", "body")
    github.assert_called_once()


# --- track orchestration ----------------------------------------------------


def test_main_runs_both_tracks_from_one_base_commit(workflow_environment):
    with patch.object(bump, "git", return_value="abc123\n") as git, patch.object(
        bump, "run_crate_track"
    ) as crates, patch.object(bump, "run_python_track") as python:
        bump.main()
    git.assert_called_once_with(workflow_environment, "rev-parse", "HEAD")
    crates.assert_called_once_with(workflow_environment, "abc123")
    python.assert_called_once_with(workflow_environment, "abc123")


def test_one_failing_track_does_not_block_the_other(workflow_environment, capsys):
    with patch.object(bump, "git", return_value="abc123\n"), patch.object(
        bump, "run_crate_track", side_effect=URLError("crates.io offline")
    ), patch.object(bump, "run_python_track") as python:
        with pytest.raises(SystemExit, match="crates"):
            bump.main()
    python.assert_called_once()
    assert "crates.io offline" in capsys.readouterr().err


def capture_bump_pr(applied):
    """Invoke the deferred edit callback while module patches are still active."""
    def recorder(root, branch, base_sha, title, body, apply_changes):
        applied.append(apply_changes(root))
        return 700
    return recorder


def test_crate_track_opens_a_pull_request(workflow_environment):
    changes = dict.fromkeys(bump.CRATES, ("0.2.0", "0.3.0"))
    applied = []
    with patch.object(bump, "published_versions", return_value={"0.2.0"}), patch.object(
        bump, "planned_bumps", return_value=changes
    ), patch.object(bump, "has_open_bump_pr", return_value=False), patch.object(
        bump, "ensure_bump_issue", return_value=123
    ) as issue, patch.object(
        bump, "apply_crate_bumps", return_value=["mssql-tds/Cargo.toml"]
    ) as apply, patch.object(
        bump, "open_bump_pr", side_effect=capture_bump_pr(applied)
    ) as pull_request:
        bump.run_crate_track(workflow_environment, "abc123")
    assert issue.call_args.args[0] == bump.CRATE_LABEL
    assert pull_request.call_args.args[1:4] == (
        bump.CRATE_BRANCH, "abc123", "Bump released TDS crates to 0.3.0"
    )
    assert "Fixes #123" in pull_request.call_args.args[4]
    # The manifests are rewritten by the callback, after the branch exists.
    assert applied == [["mssql-tds/Cargo.toml"]]
    apply.assert_called_once_with(workflow_environment, changes)


def test_python_track_opens_a_pull_request(workflow_environment):
    changes = {bump.PYTHON_DIST: ("0.3.0", "0.4.0")}
    applied = []
    with patch.object(bump, "pypi_published_versions", return_value={"0.3.0"}), patch.object(
        bump, "planned_python_bump", return_value=changes
    ), patch.object(bump, "has_open_bump_pr", return_value=False), patch.object(
        bump, "ensure_bump_issue", return_value=456
    ) as issue, patch.object(
        bump, "apply_python_bump", return_value=["mssql-py-core/pyproject.toml"]
    ) as apply, patch.object(
        bump, "open_bump_pr", side_effect=capture_bump_pr(applied)
    ) as pull_request:
        bump.run_python_track(workflow_environment, "abc123")
    assert issue.call_args.args[0] == bump.PYTHON_LABEL
    assert pull_request.call_args.args[1:4] == (
        bump.PYTHON_BRANCH, "abc123", "Bump mssql-python-rs to 0.4.0"
    )
    assert "Fixes #456" in pull_request.call_args.args[4]
    assert applied == [["mssql-py-core/pyproject.toml"]]
    apply.assert_called_once_with(workflow_environment, changes)


@pytest.mark.parametrize(
    ("track", "lookup", "planner"),
    [
        (bump.run_crate_track, "published_versions", "planned_bumps"),
        (bump.run_python_track, "pypi_published_versions", "planned_python_bump"),
    ],
)
def test_track_without_changes_writes_nothing(workflow_environment, track, lookup, planner):
    with patch.object(bump, lookup, return_value=set()), patch.object(
        bump, planner, return_value={}
    ), patch.object(bump, "has_open_bump_pr") as duplicate, patch.object(
        bump, "ensure_bump_issue"
    ) as issue, patch.object(bump, "open_bump_pr") as pull_request:
        track(workflow_environment, "abc123")
    duplicate.assert_not_called()
    issue.assert_not_called()
    pull_request.assert_not_called()


@pytest.mark.parametrize(
    ("track", "lookup", "planner", "changes"),
    [
        (bump.run_crate_track, "published_versions", "planned_bumps",
         {"mssql-tds": ("0.2.0", "0.3.0")}),
        (bump.run_python_track, "pypi_published_versions", "planned_python_bump",
         {bump.PYTHON_DIST: ("0.3.0", "0.4.0")}),
    ],
)
def test_track_skips_when_a_duplicate_pull_request_exists(
    workflow_environment, track, lookup, planner, changes
):
    with patch.object(bump, lookup, return_value={"0.2.0"}), patch.object(
        bump, planner, return_value=changes
    ), patch.object(bump, "has_open_bump_pr", return_value=True), patch.object(
        bump, "ensure_bump_issue"
    ) as issue, patch.object(bump, "open_bump_pr") as pull_request:
        track(workflow_environment, "abc123")
    issue.assert_not_called()
    pull_request.assert_not_called()


def test_crate_track_does_not_plan_on_registry_failure(workflow_environment):
    with patch.object(
        bump, "published_versions", side_effect=[{"0.2.0"}, URLError("offline")]
    ), patch.object(bump, "planned_bumps") as plan:
        with pytest.raises(URLError):
            bump.run_crate_track(workflow_environment, "abc123")
    plan.assert_not_called()


# --- workflow wiring --------------------------------------------------------


def test_workflow_scope_and_permissions():
    workflow = yaml.safe_load(WORKFLOW.read_text())
    triggers = workflow.get("on", workflow.get(True))
    assert triggers["schedule"] == [{"cron": "30 7 * * 2"}]
    assert "workflow_dispatch" in triggers
    assert workflow["permissions"] == {}
    # Pull requests are opened with a GitHub App installation token, so the
    # workflow's own GITHUB_TOKEN stays read-only.
    assert workflow["jobs"]["bump"]["permissions"] == {"contents": "read"}
    assert workflow["concurrency"]["cancel-in-progress"] is False
    steps = workflow["jobs"]["bump"]["steps"]
    assert steps[0]["id"] == "cadence"
    for step in steps[1:]:
        assert step["if"] == "steps.cadence.outputs.due == 'true'"
    assert steps[1]["id"] == "app-token"
    assert steps[1]["with"] == {
        "app-id": "${{ secrets.VERSION_BUMP_APP_ID }}",
        "private-key": "${{ secrets.VERSION_BUMP_APP_PRIVATE_KEY }}",
    }
    assert steps[2]["with"]["ref"] == "${{ github.event.repository.default_branch }}"
    assert steps[2]["with"]["token"] == "${{ steps.app-token.outputs.token }}"
    assert steps[2]["with"]["persist-credentials"] is True
    assert steps[2]["with"]["fetch-depth"] == 0
    assert steps[3]["run"] == "python3 scripts/bump-released-versions.py"
    assert steps[3]["env"]["GH_TOKEN"] == "${{ steps.app-token.outputs.token }}"
    assert steps[3]["env"]["DEFAULT_BRANCH"] == (
        "${{ github.event.repository.default_branch }}"
    )
    assert all(
        key in steps[3]["env"]
        for key in ("GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL",
                    "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL")
    )
    assert len(steps) == 4
    assert all(
        len(step["uses"].split("@")[1]) == 40 for step in steps if "uses" in step
    )


@pytest.mark.parametrize("event", ["schedule", "workflow_dispatch"])
def test_workflow_runs_on_the_first_and_third_tuesday(tmp_path, monkeypatch, event):
    workflow = yaml.safe_load(WORKFLOW.read_text())
    script = workflow["jobs"]["bump"]["steps"][0]["run"]
    code = script.removeprefix("python3 - <<'PY'\n").removesuffix("PY\n")
    output = tmp_path / "output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setenv("GITHUB_EVENT_NAME", event)

    # The cron fires only on Tuesdays, so only Tuesdays reach the guard.
    start = datetime(2026, 1, 6, 7, 30, tzinfo=timezone.utc)
    assert start.weekday() == 1
    span = [start + timedelta(weeks=week) for week in range(157)]
    # Drop the trailing partial month so every month under test is fully covered.
    tuesdays = [
        tuesday for tuesday in span
        if (tuesday.year, tuesday.month) != (span[-1].year, span[-1].month)
    ]
    with patch("datetime.datetime") as clock:
        for tuesday in tuesdays:
            clock.now.return_value = tuesday
            exec(code, {})

    verdicts = output.read_text().splitlines()
    assert len(verdicts) == len(tuesdays)
    if event != "schedule":
        assert set(verdicts) == {"due=true"}
        return
    due = [day for day, line in zip(tuesdays, verdicts) if line == "due=true"]
    assert all(
        (tuesday in due) == (tuesday.day <= 7 or 15 <= tuesday.day <= 21)
        for tuesday in tuesdays
    )
    # Exactly the 1st and 3rd Tuesday of every fully covered month.
    months = Counter((tuesday.year, tuesday.month) for tuesday in due)
    assert set(months.values()) == {2}
    ordinals = Counter((tuesday.day - 1) // 7 for tuesday in due)
    assert set(ordinals) == {0, 2}

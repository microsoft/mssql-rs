# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import importlib.util
import io
import json
import re
import shlex
import shutil
import subprocess
import base64
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

PIPELINE = ROOT / ".pipeline" / "bump-released-versions.yml"
BASH = shutil.which("bash")


@pytest.fixture(autouse=True)
def github_environment(monkeypatch):
    monkeypatch.setenv("GITHUB_REPOSITORY", "microsoft/mssql-rs")
    monkeypatch.setenv("DEFAULT_BRANCH", "main")
    monkeypatch.setenv("GH_TOKEN", "ghs_testtoken")


# --- version planning -------------------------------------------------------


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


@pytest.mark.parametrize(("current", "expected"), [("0.1.7", "0.2.0"), ("1.9.3", "1.10.0")])
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


def test_pypi_releases_are_published_versions():
    response = io.BytesIO(json.dumps({"releases": {"0.2.0": [], "0.3.0": []}}).encode())
    with patch.object(bump, "urlopen", return_value=response) as request:
        assert bump.pypi_published_versions(bump.PYTHON_DIST) == {"0.2.0", "0.3.0"}
    assert request.call_args.args[0].full_url == (
        f"https://pypi.org/pypi/{bump.PYTHON_DIST}/json"
    )


@pytest.mark.parametrize("lookup", [bump.published_versions, bump.pypi_published_versions])
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


# --- GitHub REST layer ------------------------------------------------------


def api_response(payload, status=200):
    body = json.dumps(payload).encode() if payload is not None else b""
    response = Mock()
    response.read.return_value = body
    response.status = status
    response.__enter__ = Mock(return_value=response)
    response.__exit__ = Mock(return_value=False)
    return response


def test_github_api_sends_token_and_version_headers():
    with patch.object(bump, "urlopen", return_value=api_response({"ok": True})) as call:
        status, body = bump.github_api("GET", "/repos/microsoft/mssql-rs/pulls")
    request = call.call_args.args[0]
    assert status == 200 and body == {"ok": True}
    assert request.full_url == "https://api.github.com/repos/microsoft/mssql-rs/pulls"
    assert request.get_header("Authorization") == "Bearer ghs_testtoken"
    assert request.get_header("X-github-api-version") == "2022-11-28"
    assert request.get_method() == "GET"


def test_github_api_posts_json_body():
    with patch.object(bump, "urlopen", return_value=api_response({"number": 7}, 201)) as call:
        status, body = bump.github_api("POST", "/repos/x/y/issues", {"title": "hi"})
    request = call.call_args.args[0]
    assert (status, body) == (201, {"number": 7})
    assert json.loads(request.data) == {"title": "hi"}
    assert request.get_method() == "POST"


def test_github_api_tolerates_allowed_status():
    error = HTTPError("https://api.github.com", 422, "exists", {}, None)
    with patch.object(bump, "urlopen", side_effect=error):
        assert bump.github_api("POST", "/repos/x/y/labels", {}, allow_status=(422,)) == (422, None)


def test_github_api_raises_with_detail_on_unexpected_status():
    error = HTTPError("https://api.github.com", 403, "forbidden", {}, io.BytesIO(b'{"message":"no"}'))
    with patch.object(bump, "urlopen", side_effect=error):
        with pytest.raises(RuntimeError, match="HTTP 403"):
            bump.github_api("POST", "/repos/x/y/pulls", {})


def test_github_api_retries_transient_status():
    error = HTTPError("https://api.github.com", 502, "bad gateway", {}, None)
    with patch.object(bump, "urlopen", side_effect=[error, api_response({"ok": 1})]):
        with patch.object(bump.time, "sleep"):
            assert bump.github_api("GET", "/x")[1] == {"ok": 1}


def test_github_paged_stops_on_short_page():
    pages = [api_response([{"n": i} for i in range(100)]), api_response([{"n": 100}])]
    with patch.object(bump, "urlopen", side_effect=pages) as call:
        assert len(bump.github_paged("/repos/x/y/pulls?state=open")) == 101
    assert "per_page=100&page=2" in call.call_args.args[0].full_url
    # The existing query string must be preserved, not replaced.
    assert "state=open" in call.call_args.args[0].full_url


def test_github_paged_uses_question_mark_when_no_query():
    with patch.object(bump, "urlopen", return_value=api_response([])) as call:
        bump.github_paged("/repos/x/y/labels")
    assert call.call_args.args[0].full_url.endswith("/labels?per_page=100&page=1")


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
    with pytest.raises(ValueError, match=r"no \[package\] version field"):
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


def pulls(*entries):
    return [
        {"number": n, "title": t, "body": b, "head": {"ref": ref}}
        for n, t, b, ref in entries
    ]


def test_has_open_bump_pr():
    listed = pulls(
        (584, "Fix mssql-tds docs", "Mentions mssql-tds only", "docs"),
        (597, "Bump mssql-tds", "- `mssql-tds`: `0.1.7` -> `0.2.0`", "manual-bump"),
    )
    with patch.object(bump, "github_paged", return_value=listed) as paged:
        assert bump.has_open_bump_pr({"mssql-tds": ("0.1.7", "0.2.0")}, bump.CRATE_BRANCH)
    paged.assert_called_once_with("/repos/microsoft/mssql-rs/pulls?state=open&base=main")


def test_our_own_pr_is_not_treated_as_a_duplicate():
    listed = pulls((600, "Bump", "- `mssql-tds`: `0.1.7` -> `0.2.0`", bump.CRATE_BRANCH))
    with patch.object(bump, "github_paged", return_value=listed):
        assert not bump.has_open_bump_pr(
            {"mssql-tds": ("0.1.7", "0.2.0")}, bump.CRATE_BRANCH
        )


def test_open_bump_pr_requires_every_selected_crate():
    listed = pulls((600, "Bump", "- `mssql-tds`: `0.1.7` -> `0.2.0`", "manual-bump"))
    with patch.object(bump, "github_paged", return_value=listed):
        assert not bump.has_open_bump_pr(
            dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0")), bump.CRATE_BRANCH
        )


def test_open_bump_pr_requires_exact_transition():
    listed = pulls((601, "Bump to 0.2.0", "- `mssql-tds`: `0.1.6` -> `0.2.0`", "manual"))
    with patch.object(bump, "github_paged", return_value=listed):
        assert not bump.has_open_bump_pr(
            {"mssql-tds": ("0.1.7", "0.2.0")}, bump.CRATE_BRANCH
        )


def test_open_bump_pr_requires_summary_line_format():
    listed = pulls((602, "Notes", "Compatibility notes for mssql-tds 0.1.7 and 0.2.0", "notes"))
    with patch.object(bump, "github_paged", return_value=listed):
        assert not bump.has_open_bump_pr(
            {"mssql-tds": ("0.1.7", "0.2.0")}, bump.CRATE_BRANCH
        )


# --- branch and pull request creation ---------------------------------------


def record_git(applied=None):
    calls = []

    def run(args, **kwargs):
        calls.append(args)
        return subprocess.CompletedProcess(args, 0, stdout="")

    return calls, run


def test_open_bump_pr_branches_before_editing_and_creates(tmp_path):
    calls, run = record_git()
    edited_after = []
    with patch.object(bump.subprocess, "run", side_effect=run), patch.object(
        bump, "github_paged", return_value=[]
    ), patch.object(
        bump, "github_api",
        return_value=(201, {"number": 700, "html_url": "https://example/700"}),
    ) as api:
        bump.open_bump_pr(
            tmp_path, bump.CRATE_BRANCH, "abc123", "Bump released TDS crates to 0.3.0", "body",
            lambda root: (edited_after.append(list(calls)), ["mssql-tds/Cargo.toml"])[1],
        )
    # Edits must land only after the branch exists, so a previous track's commit
    # on another branch cannot leak into this one.
    assert edited_after[0] == [["git", "switch", "--force-create", bump.CRATE_BRANCH, "abc123"]]
    assert calls[1] == ["git", "add", "--", "mssql-tds/Cargo.toml"]
    assert calls[2] == ["git", "commit", "--message", "Bump released TDS crates to 0.3.0"]
    assert calls[3] == [
        "git", "push", "--force", "origin",
        f"{bump.CRATE_BRANCH}:refs/heads/{bump.CRATE_BRANCH}",
    ]
    method, path = api.call_args.args[0], api.call_args.args[1]
    assert (method, path) == ("POST", "/repos/microsoft/mssql-rs/pulls")
    assert api.call_args.args[2]["base"] == "main"
    assert api.call_args.args[2]["head"] == bump.CRATE_BRANCH


def test_open_bump_pr_updates_existing_pull_request(tmp_path):
    _calls, run = record_git()
    with patch.object(bump.subprocess, "run", side_effect=run), patch.object(
        bump, "github_paged", return_value=[{"number": 694}]
    ), patch.object(bump, "github_api", return_value=(200, {})) as api:
        assert bump.open_bump_pr(
            tmp_path, bump.PYTHON_BRANCH, "abc123", "Bump mssql-python-rs to 0.4.0", "body",
            lambda root: ["mssql-py-core/pyproject.toml"],
        ) == 694
    assert api.call_args.args[:2] == ("PATCH", "/repos/microsoft/mssql-rs/pulls/694")


def test_pull_request_body_links_an_issue_for_the_linked_issue_check():
    body = bump.pull_request_body("- `mssql-tds`: `0.2.0` -> `0.3.0`", 693, "Extra note.")
    assert "Fixes #693" in body
    assert "## Description" in body
    assert "## Related Issues" in body
    assert "## Checklist" in body
    assert "Extra note." in body
    # PR Linked Issue Check rejects a body with no `#<number>` or ADO work item link.
    assert re.search(r"#\d+", body)


# --- tracking issues --------------------------------------------------------


def test_issue_created_with_label_then_reused_and_updated():
    state = {}

    def paged(path):
        assert path == f"/repos/microsoft/mssql-rs/issues?state=open&labels={bump.CRATE_LABEL}"
        return [
            {"number": 10, "body": "Unrelated"},
            {"number": 11, "body": None},
            # A pull request also surfaces on the issues endpoint and must be ignored.
            {"number": 12, "body": bump.CRATE_MARKER, "pull_request": {"url": "x"}},
            *([state] if state else []),
        ]

    writes = []

    def api(method, path, payload=None, allow_status=()):
        writes.append((method, path))
        if path.endswith("/labels"):
            assert payload["name"] == bump.CRATE_LABEL
            assert 422 in allow_status
            return 201, None
        if method == "POST":
            state.update({"number": 123, **payload})
            return 201, dict(state)
        state.update(payload)
        return 200, dict(state)

    title = "Bump released crates to the next minor version"
    first = {"mssql-tds": ("0.1.7", "0.2.0")}
    both = dict.fromkeys(bump.CRATES, ("0.1.7", "0.2.0"))
    with patch.object(bump, "github_paged", side_effect=paged), patch.object(
        bump, "github_api", side_effect=api
    ):
        for _ in range(2):
            assert bump.ensure_bump_issue(
                bump.CRATE_LABEL, bump.CRATE_MARKER, title,
                bump.crate_issue_body("- `mssql-tds`: `0.1.7` -> `0.2.0`", first),
            ) == 123
        # Created once, then reused without a second write.
        assert [m for m, _ in writes] == ["POST", "POST"]
        assert bump.ensure_bump_issue(
            bump.CRATE_LABEL, bump.CRATE_MARKER, title,
            bump.crate_issue_body("Both bumps", both),
        ) == 123
    assert writes[-1] == ("PATCH", "/repos/microsoft/mssql-rs/issues/123")
    body = state["body"]
    assert body.startswith(bump.CRATE_MARKER)
    assert "Both bumps" in body
    assert "mssql-tds/Cargo.toml" in body
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


def test_duplicate_tracking_issues_fail_without_writing():
    listed = [{"number": n, "body": bump.CRATE_MARKER} for n in (123, 456)]
    with patch.object(bump, "github_paged", return_value=listed), patch.object(
        bump, "github_api"
    ) as api:
        with pytest.raises(ValueError, match="Multiple open"):
            bump.ensure_bump_issue(bump.CRATE_LABEL, bump.CRATE_MARKER, "Bump", "body")
    api.assert_not_called()


def test_issue_api_errors_propagate():
    with patch.object(bump, "github_paged", return_value=[]), patch.object(
        bump, "github_api", side_effect=RuntimeError("HTTP 403")
    ):
        with pytest.raises(RuntimeError, match="HTTP 403"):
            bump.ensure_bump_issue(bump.CRATE_LABEL, bump.CRATE_MARKER, "Bump", "body")


# --- track orchestration ----------------------------------------------------


def test_main_runs_both_tracks_from_one_base_commit(tmp_path, monkeypatch):
    monkeypatch.setattr(bump, "__file__", str(tmp_path / "scripts" / "bump.py"))
    with patch.object(bump, "git", return_value="abc123\n") as git, patch.object(
        bump, "run_crate_track"
    ) as crates, patch.object(bump, "run_python_track") as python:
        bump.main()
    git.assert_called_once_with(tmp_path, "rev-parse", "HEAD")
    crates.assert_called_once_with(tmp_path, "abc123")
    python.assert_called_once_with(tmp_path, "abc123")


def test_one_failing_track_does_not_block_the_other(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(bump, "__file__", str(tmp_path / "scripts" / "bump.py"))
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


def test_crate_track_opens_a_pull_request(tmp_path):
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
        bump.run_crate_track(tmp_path, "abc123")
    assert issue.call_args.args[0] == bump.CRATE_LABEL
    assert pull_request.call_args.args[1:4] == (
        bump.CRATE_BRANCH, "abc123", "Bump released TDS crates to 0.3.0"
    )
    assert "Fixes #123" in pull_request.call_args.args[4]
    assert applied == [["mssql-tds/Cargo.toml"]]
    apply.assert_called_once_with(tmp_path, changes)


def test_python_track_opens_a_pull_request(tmp_path):
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
        bump.run_python_track(tmp_path, "abc123")
    assert issue.call_args.args[0] == bump.PYTHON_LABEL
    assert pull_request.call_args.args[1:4] == (
        bump.PYTHON_BRANCH, "abc123", "Bump mssql-python-rs to 0.4.0"
    )
    assert "Fixes #456" in pull_request.call_args.args[4]
    assert applied == [["mssql-py-core/pyproject.toml"]]
    apply.assert_called_once_with(tmp_path, changes)


@pytest.mark.parametrize(("track", "lookup", "planner"), [
    (bump.run_crate_track, "published_versions", "planned_bumps"),
    (bump.run_python_track, "pypi_published_versions", "planned_python_bump"),
])
def test_track_without_changes_writes_nothing(tmp_path, track, lookup, planner):
    with patch.object(bump, lookup, return_value=set()), patch.object(
        bump, planner, return_value={}
    ), patch.object(bump, "has_open_bump_pr") as duplicate, patch.object(
        bump, "ensure_bump_issue"
    ) as issue, patch.object(bump, "open_bump_pr") as pull_request:
        track(tmp_path, "abc123")
    duplicate.assert_not_called()
    issue.assert_not_called()
    pull_request.assert_not_called()


@pytest.mark.parametrize(("track", "lookup", "planner", "changes"), [
    (bump.run_crate_track, "published_versions", "planned_bumps",
     {"mssql-tds": ("0.2.0", "0.3.0")}),
    (bump.run_python_track, "pypi_published_versions", "planned_python_bump",
     {bump.PYTHON_DIST: ("0.3.0", "0.4.0")}),
])
def test_track_skips_when_a_duplicate_pull_request_exists(
    tmp_path, track, lookup, planner, changes
):
    with patch.object(bump, lookup, return_value={"0.2.0"}), patch.object(
        bump, planner, return_value=changes
    ), patch.object(bump, "has_open_bump_pr", return_value=True), patch.object(
        bump, "ensure_bump_issue"
    ) as issue, patch.object(bump, "open_bump_pr") as pull_request:
        track(tmp_path, "abc123")
    issue.assert_not_called()
    pull_request.assert_not_called()


def test_crate_track_does_not_plan_on_registry_failure(tmp_path):
    with patch.object(
        bump, "published_versions", side_effect=[{"0.2.0"}, URLError("offline")]
    ), patch.object(bump, "planned_bumps") as plan:
        with pytest.raises(URLError):
            bump.run_crate_track(tmp_path, "abc123")
    plan.assert_not_called()


# --- pipeline wiring --------------------------------------------------------


def load_pipeline():
    return yaml.safe_load(PIPELINE.read_text(encoding="utf-8"))


def test_pipeline_runs_only_on_its_own_schedule():
    pipeline = load_pipeline()
    assert pipeline["trigger"] == "none"
    assert pipeline["pr"] == "none"
    schedule = pipeline["schedules"]
    assert len(schedule) == 1
    assert schedule[0]["cron"] == "30 7 * * 2"
    assert schedule[0]["branches"]["include"] == ["main"]
    assert schedule[0]["always"] is True


def test_pipeline_checkout_can_push_and_authenticate():
    steps = load_pipeline()["steps"]
    checkout = steps[0]
    assert checkout["checkout"] == "self"
    # Pushing a new branch needs full history and the persisted credential.
    assert checkout["persistCredentials"] is True
    assert checkout["fetchDepth"] == 0


def test_pipeline_gates_every_writing_step_on_the_cadence_check():
    steps = load_pipeline()["steps"]
    cadence = next(s for s in steps if s.get("displayName") == "Check cadence")
    assert "condition" not in cadence
    writing = [s for s in steps if s.get("displayName") != "Check cadence" and "script" in s]
    assert writing, "expected steps after the cadence check"
    for step in writing:
        assert step["condition"] == "eq(variables['due'], 'true')"


def test_pipeline_masks_the_token_and_passes_it_to_the_script():
    steps = load_pipeline()["steps"]
    extract = next(s for s in steps if "Read GitHub token" in s.get("displayName", ""))
    # issecret keeps the token out of every downstream log.
    assert "issecret=true" in extract["script"]
    assert "task.setvariable variable=ghToken" in extract["script"]
    run = next(s for s in steps if s.get("displayName") == "Open version bump pull requests")
    assert run["script"].strip() == "python3 scripts/bump-released-versions.py"
    assert run["env"]["GH_TOKEN"] == "$(ghToken)"
    assert run["env"]["GITHUB_REPOSITORY"] == "microsoft/mssql-rs"
    assert run["env"]["DEFAULT_BRANCH"] == "main"
    for key in ("GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL",
                "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL"):
        assert run["env"][key]


def test_pipeline_token_extraction_finds_the_repo_scoped_key(tmp_path):
    """Run the real sed against real `git config --list` output.

    The credential key is repo-scoped (http.https://github.com/microsoft/mssql-rs
    .extraheader), not org-wide. An earlier spike failed against a hardcoded
    org-wide key, so this extraction must discover it.
    """
    steps = load_pipeline()["steps"]
    extract = next(s for s in steps if "Read GitHub token" in s.get("displayName", ""))
    sed_line = next(line for line in extract["script"].splitlines() if "extraheader" in line)
    sed_expression = sed_line.split("|", 1)[1].split("| head", 1)[0].strip()

    token = "ghs_realtokenvalue"
    header = f"AUTHORIZATION: basic {base64.b64encode(f'x-access-token:{token}'.encode()).decode()}"
    config = "\n".join([
        "http.version=HTTP/1.1",
        f"http.https://github.com/microsoft/mssql-rs.extraheader={header}",
        "user.name=someone",
    ])
    script = f"""
    set -euo pipefail
    KEY="$(printf '%s\\n' {shlex.quote(config)} | {sed_expression} | head -n1)"
    echo "KEY=$KEY"
    HEADER={shlex.quote(header)}
    DECODED="$(printf '%s' "${{HEADER##* }}" | base64 -d)"
    echo "TOKEN=${{DECODED#*:}}"
    """
    result = subprocess.run([BASH, "-c", script], capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    assert "KEY=http.https://github.com/microsoft/mssql-rs.extraheader" in result.stdout
    assert f"TOKEN={token}" in result.stdout


@pytest.mark.parametrize("reason", ["Schedule", "Manual"])
def test_cadence_guard_selects_the_first_and_third_tuesday(tmp_path, reason):
    """Execute the pipeline's own bash guard for every Tuesday over three years."""
    steps = load_pipeline()["steps"]
    guard = next(s for s in steps if s.get("displayName") == "Check cadence")

    start = datetime(2026, 1, 6, 7, 30, tzinfo=timezone.utc)
    assert start.weekday() == 1
    span = [start + timedelta(weeks=week) for week in range(157)]
    tuesdays = [t for t in span if (t.year, t.month) != (span[-1].year, span[-1].month)]

    output = tmp_path / "vso.txt"
    verdicts = []
    for tuesday in tuesdays:
        # Stub `date` so the guard reads our simulated day, and capture the
        # ##vso logging command instead of letting it reach a real agent.
        script = (
            f"date() {{ echo {tuesday.day}; }}\n"
            f"export BUILD_REASON={reason}\n"
            + guard["script"]
        )
        result = subprocess.run([BASH, "-c", script], capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        verdict = re.search(r"variable=due\](\w+)", result.stdout)
        assert verdict, result.stdout
        verdicts.append(verdict.group(1))

    due = [t for t, v in zip(tuesdays, verdicts) if v == "true"]
    if reason != "Schedule":
        assert len(due) == len(tuesdays)
        return
    # Exactly the 1st and 3rd Tuesday of every fully covered month.
    assert set(Counter((t.year, t.month) for t in due).values()) == {2}
    assert set(Counter((t.day - 1) // 7 for t in due)) == {0, 2}


def test_pipeline_skips_the_codeql_decorator():
    variables = {v["name"]: v["value"] for v in load_pipeline()["variables"]}
    assert variables["Codeql.SkipTaskAutoInjection"] is True

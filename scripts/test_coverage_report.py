# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Regression tests for the PR coverage reporting trust boundaries."""

import importlib.util
from fnmatch import fnmatchcase
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import stat
import subprocess
import threading
import time
from types import SimpleNamespace
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlparse
import xml.etree.ElementTree as ET
import zipfile

import pytest
import yaml


ROOT = Path(__file__).parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "coverage-report.py"
SPEC = importlib.util.spec_from_file_location("coverage_report", SCRIPT)
assert SPEC and SPEC.loader
coverage_report = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(coverage_report)
POLL_SPEC = importlib.util.spec_from_file_location(
    "wait_for_coverage", ROOT / ".github" / "scripts" / "wait-for-coverage.py"
)
assert POLL_SPEC and POLL_SPEC.loader
polling = importlib.util.module_from_spec(POLL_SPEC)
POLL_SPEC.loader.exec_module(polling)
MEMBER = "CoberturaCoverage/cobertura.xml"
XML = '<coverage line-rate="0.85"><packages/></coverage>'


def archive_with(tmp_path, content=XML, name=MEMBER):
    archive = tmp_path / "coverage.zip"
    if isinstance(name, str):
        member = zipfile.ZipInfo(name)
        member.filename = name
    else:
        member = name
    with zipfile.ZipFile(archive, "w") as output:
        output.writestr(member, content)
    return archive


def fake_diff_cover(monkeypatch, source, value=85, summary="Coverage report"):
    def run(command, *, cwd, check):
        assert cwd == source
        assert check is True
        assert command[1:4] == ["-I", "-m", "diff_cover.diff_cover_tool"]
        assert "--compare-branch=origin/main" in command
        assert "--ignore-staged" in command
        assert "--ignore-unstaged" in command
        assert not any(arg.startswith("--fail-under") for arg in command)
        formats = command[command.index("--format") + 1].split(",")
        paths = [Path(item.split(":", 1)[1]) for item in formats]
        assert all(not path.is_relative_to(source) and not path.exists() for path in paths)
        paths[0].write_text(json.dumps({"total_percent_covered": value}), encoding="utf-8")
        paths[1].write_text(summary, encoding="utf-8")
    monkeypatch.setattr(coverage_report.subprocess, "run", run)


@pytest.mark.parametrize("directory_entry", [False, True])
def test_extracts_only_expected_xml(tmp_path, directory_entry):
    archive = archive_with(tmp_path)
    if directory_entry:
        with zipfile.ZipFile(archive, "a") as output:
            output.mkdir("CoberturaCoverage/")
    destination = tmp_path / "coverage.xml"
    coverage_report.extract_coverage(archive, destination)
    assert destination.read_text() == XML


@pytest.mark.parametrize("name", [
    "../cobertura.xml",
    "/cobertura.xml",
    "CoberturaCoverage/../cobertura.xml",
    "CoberturaCoverage\\cobertura.xml",
    ".github/actions/post-coverage-comment/action.yml",
    ".git/config",
    "patch-coverage.md",
    "cobertura.xml",
])
def test_rejects_unexpected_archive_paths(tmp_path, name):
    archive = archive_with(tmp_path, name=name)
    destination = tmp_path / "coverage.xml"
    with pytest.raises(ValueError, match="Unexpected coverage archive member"):
        coverage_report.extract_coverage(archive, destination)
    assert not destination.exists()


@pytest.mark.parametrize("kind", [stat.S_IFLNK, stat.S_IFIFO, stat.S_IFDIR, stat.S_IFCHR])
def test_rejects_non_regular_xml(tmp_path, kind):
    member = zipfile.ZipInfo(MEMBER)
    member.create_system = 3
    member.external_attr = (kind | 0o644) << 16
    archive = archive_with(tmp_path, name=member)
    with pytest.raises(ValueError, match="Unexpected coverage archive member"):
        coverage_report.extract_coverage(archive, tmp_path / "coverage.xml")


def test_rejects_duplicate_xml(tmp_path):
    archive = archive_with(tmp_path)
    with zipfile.ZipFile(archive, "a") as output, pytest.warns(UserWarning):
        output.writestr(MEMBER, XML)
    with pytest.raises(ValueError, match="exactly one"):
        coverage_report.extract_coverage(archive, tmp_path / "coverage.xml")


def test_rejects_missing_xml(tmp_path):
    archive = tmp_path / "empty.zip"
    with zipfile.ZipFile(archive, "w"):
        pass
    with pytest.raises(ValueError, match="exactly one"):
        coverage_report.extract_coverage(archive, tmp_path / "coverage.xml")


@pytest.mark.parametrize("value", ["99\nINJECTED=1", None, True, {}, [], -1, 101, float("nan"), float("inf")])
def test_rejects_invalid_patch_percentages(tmp_path, monkeypatch, value):
    source = tmp_path / "source"
    source.mkdir()
    fake_diff_cover(monkeypatch, source, value=value)
    with pytest.raises(ValueError, match="Invalid coverage value"):
        coverage_report.generate_report(archive_with(tmp_path), source, "123", tmp_path)


@pytest.mark.parametrize("value", [0, 85.5, 100])
def test_reports_valid_coverage_without_using_stale_files(tmp_path, monkeypatch, value):
    source = tmp_path / "source"
    source.mkdir()
    (source / "patch-coverage.json").write_text('{"total_percent_covered":"99\\nINJECTED=1"}')
    (source / "patch-coverage.md").write_text("EOF\nINJECTED=1\n")
    fake_diff_cover(monkeypatch, source, value=value)
    report = coverage_report.generate_report(archive_with(tmp_path), source, "123", tmp_path)
    assert report == {
        "coverage_percentage": "85.0%",
        "patch_coverage_pct": f"{value:g}%",
        "patch_coverage_summary": "Coverage report",
        "ado_url": "https://dev.azure.com/sqlclientdrivers/public/_build/results?buildId=123",
    }
    assert not list(tmp_path.glob("coverage-report-*"))


@pytest.mark.parametrize("xml", [
    '<coverage line-rate="0.8">',
    '<coverage line-rate="NaN"/>',
    '<coverage line-rate="1.1"/>',
    '<coverage line-rate="-0.1"/>',
    '<other line-rate="0.8"/>',
])
def test_invalid_xml_never_uses_stale_reports(tmp_path, monkeypatch, xml):
    source = tmp_path / "source"
    source.mkdir()
    (source / "patch-coverage.md").write_text("EOF\nINJECTED=1")
    def unexpected_run(*args, **kwargs):
        pytest.fail("Malformed coverage must fail before invoking diff-cover")
    monkeypatch.setattr(coverage_report.subprocess, "run", unexpected_run)
    with pytest.raises((ValueError, ET.ParseError)):
        coverage_report.generate_report(archive_with(tmp_path, xml), source, "123", tmp_path)


def test_diff_cover_failure_is_not_swallowed(tmp_path, monkeypatch):
    source = tmp_path / "source"
    source.mkdir()
    (source / "patch-coverage.md").write_text("EOF\nINJECTED=1")
    def fail(command, **kwargs):
        raise subprocess.CalledProcessError(1, command)
    monkeypatch.setattr(coverage_report.subprocess, "run", fail)
    with pytest.raises(subprocess.CalledProcessError):
        coverage_report.generate_report(archive_with(tmp_path), source, "123", tmp_path)
    assert not list(tmp_path.glob("coverage-report-*"))


def test_missing_overall_rate_is_explicitly_unavailable(tmp_path, monkeypatch):
    source = tmp_path / "source"
    source.mkdir()
    fake_diff_cover(monkeypatch, source)
    report = coverage_report.generate_report(archive_with(tmp_path, "<coverage/>"), source, "123", tmp_path)
    assert report["coverage_percentage"] == "N/A"


@pytest.mark.parametrize("build_id", ["0", "-1", "12\nINJECTED=1", "12&other=1", "1.5"])
def test_rejects_invalid_build_id(tmp_path, build_id):
    with pytest.raises(ValueError, match="Invalid Azure DevOps build ID"):
        coverage_report.generate_report(tmp_path / "absent.zip", tmp_path, build_id, tmp_path)


def test_markdown_is_data_not_environment_commands(tmp_path):
    output = tmp_path / "output"
    payload = "report\nEOF\nINJECTED=1\nreport=spoofed\r\n\u2028"
    coverage_report.write_output(output, {"patch_coverage_summary": payload})
    lines = output.read_text().splitlines()
    assert len(lines) == 1
    assert lines[0].startswith("report=")
    assert json.loads(lines[0].split("=", 1)[1])["patch_coverage_summary"] == payload


def test_long_report_has_visible_truncation(tmp_path, monkeypatch):
    source = tmp_path / "source"
    source.mkdir()
    fake_diff_cover(monkeypatch, source, summary="x" * 60000)
    report = coverage_report.generate_report(archive_with(tmp_path), source, "123", tmp_path)
    assert report["patch_coverage_summary"].startswith("x" * 50000)
    assert report["patch_coverage_summary"].endswith("_Report truncated to fit a PR comment._")


def workflow():
    return yaml.safe_load((ROOT / ".github" / "workflows" / "pr-code-coverage.yml").read_text(encoding="utf-8"))


def test_only_trusted_posting_job_has_write_permission():
    config = workflow()
    assert config["permissions"] == {}
    report = config["jobs"]["coverage-report"]
    post = config["jobs"]["post-comment"]
    assert set(report["permissions"].values()) == {"read"}
    assert not any(step.get("uses", "").startswith("./") for step in report["steps"])
    assert post["permissions"] == {"contents": "read", "pull-requests": "write"}
    assert post["needs"] == "coverage-report"
    checkouts = [step for step in post["steps"] if step.get("uses", "").startswith("actions/checkout@")]
    assert len(checkouts) == 1
    assert checkouts[0]["with"]["ref"] == "${{ github.workflow_sha }}"
    assert checkouts[0]["with"]["persist-credentials"] is False
    assert not any("run" in step for step in post["steps"])
    authorize = config["jobs"]["authorize"]
    assert "github.event.pull_request.head.repo.full_name == github.repository" in authorize["if"]
    assert "github.event_name == 'issue_comment'" in authorize["if"]
    assert set(authorize["permissions"].values()) == {"read"}
    assert report["needs"] == "authorize"


def test_reporting_uses_trusted_tools_and_immutable_pr_head():
    steps = {step["name"]: step for step in workflow()["jobs"]["coverage-report"]["steps"]}
    assert steps["Checkout trusted reporting tools"]["with"]["ref"] == "${{ github.workflow_sha }}"
    assert steps["Checkout trusted reporting tools"]["with"]["path"] == "tooling"
    assert steps["Checkout coverage source"]["with"]["ref"] == "${{ steps.build.outputs.head_sha }}"
    assert steps["Checkout coverage source"]["with"]["path"] == "source"
    names = list(steps)
    assert names.index("Wait for latest ADO coverage artifact") < names.index("Checkout coverage source")
    for step in steps.values():
        if step.get("uses", "").startswith("actions/checkout@"):
            assert step["with"]["persist-credentials"] is False
        assert "GITHUB_ENV" not in step.get("run", "")
    context = workflow()["jobs"]["authorize"]["steps"][0]["with"]["script"]
    assert "getCollaboratorPermissionLevel" in context
    assert "['admin', 'maintain', 'write'].includes(data.permission)" in context
    assert 'python -I tooling/.github/scripts/coverage-report.py' in steps["Run diff-cover"]["run"]
    assert "--require-hashes" in steps["Install pinned reporting dependencies"]["run"]
    assert "--only-binary=:all:" in steps["Install pinned reporting dependencies"]["run"]


def test_build_and_download_are_bound_to_expected_provenance():
    steps = {step["name"]: step for step in workflow()["jobs"]["coverage-report"]["steps"]}
    build = steps["Wait for latest ADO coverage artifact"]
    assert build["env"] == {
        "PR_NUMBER": "${{ needs.authorize.outputs.pr_number }}",
        "GH_TOKEN": "${{ github.token }}",
    }
    assert "python -I tooling/.github/scripts/wait-for-coverage.py" in build["run"]
    assert steps["Download Cobertura coverage artifact"]["env"]["BUILD_ID"] == "${{ steps.build.outputs.build_id }}"
    download = steps["Download Cobertura coverage artifact"]["run"]
    assert ".resource.downloadUrl" not in download
    assert "unzip" not in download
    assert "&artifactName=CoberturaCoverage&%24format=zip" in download
    assert '"$RUNNER_TEMP/coverage-artifact.zip"' in download


def test_security_regressions_run_in_validation():
    pipeline = (ROOT / ".pipeline" / "templates" / "validation-stages.yml").read_text(encoding="utf-8")
    command = next(line for line in pipeline.splitlines() if "python -m pytest" in line)
    assert "scripts/test_coverage_report.py" in command.split()


def run_workflow_script(script, **values):
    harness = r"""
const input = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
const outputs = {};
const notices = [];
const pr = { number: input.number ?? 710, head: { sha: input.currentHead ?? 'a'.repeat(40) } };
const context = {
  eventName: input.eventName ?? 'issue_comment',
  repo: { owner: 'microsoft', repo: 'mssql-rs' },
  issue: { number: 710 },
  payload: { comment: { user: { login: 'test-contributor' }, author_association: input.association } }
};
if (context.eventName === 'pull_request') context.payload.pull_request = pr;
const github = { rest: {
  repos: { getCollaboratorPermissionLevel: async () => ({ data: { permission: input.permission } }) },
  pulls: { get: async () => {
    if (input.apiError) throw new Error('API failed');
    return { data: pr };
  } }
} };
const core = { setOutput: (key, value) => { outputs[key] = value; }, notice: message => notices.push(message) };
Object.assign(process.env, input.env ?? {});
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
(async () => {
  try {
    await new AsyncFunction('context', 'github', 'core', input.script)(context, github, core);
    console.log(JSON.stringify({ outputs, notices }));
  } catch (error) {
    console.log(JSON.stringify({ outputs, notices, error: error.message }));
  }
})();
"""
    result = subprocess.run(
        ["node", "-e", harness], input=json.dumps({"script": script, **values}),
        capture_output=True, text=True, check=True,
    )
    return json.loads(result.stdout)


@pytest.mark.parametrize("association", ["MEMBER", "COLLABORATOR"])
@pytest.mark.parametrize("permission", ["read", "triage", "write", "maintain", "admin"])
def test_comment_authorization_precedes_shared_concurrency(association, permission):
    config = workflow()
    authorize = config["jobs"]["authorize"]
    report = config["jobs"]["coverage-report"]
    result = run_workflow_script(
        authorize["steps"][0]["with"]["script"], association=association, permission=permission,
    )
    if permission in ("read", "triage"):
        assert result["error"] == "/coverage requires repository write permission"
        assert result["outputs"] == {}
    else:
        assert "error" not in result
        assert result["outputs"]["pr_number"] == 710
    assert "github.event_name == 'issue_comment' && format('-{0}', github.run_id)" in config["concurrency"]["group"]
    assert report["needs"] == "authorize"
    assert report.get("if", "success()") == "success()"
    assert report["concurrency"] == {
        "group": "pr-coverage-report-${{ needs.authorize.outputs.pr_number }}-${{ github.event_name }}",
        "cancel-in-progress": True,
    }
    assert authorize["outputs"]["pr_number"] == "${{ steps.prctx.outputs.pr_number }}"


def test_same_repository_pr_context_remains_automatic():
    script = workflow()["jobs"]["authorize"]["steps"][0]["with"]["script"]
    result = run_workflow_script(script, eventName="pull_request")
    assert result["outputs"] == {"pr_number": 710}
    assert "error" not in result


@pytest.mark.parametrize("current_head,expected", [("a" * 40, True), ("b" * 40, False)])
def test_posting_skips_head_changed_between_jobs(current_head, expected):
    config = workflow()
    report = config["jobs"]["coverage-report"]
    post = config["jobs"]["post-comment"]
    guard = post["steps"][-2]
    comment = post["steps"][-1]
    assert report["outputs"]["head_sha"] == "${{ steps.build.outputs.head_sha }}"
    assert guard["env"]["REPORT_HEAD_SHA"] == "${{ needs.coverage-report.outputs.head_sha }}"
    assert guard["env"]["PR_NUMBER"] == "${{ needs.coverage-report.outputs.pr_number }}"
    assert comment["if"] == "steps.head.outputs.current == 'true'"
    result = run_workflow_script(
        guard["with"]["script"], currentHead=current_head,
        env={"PR_NUMBER": "710", "REPORT_HEAD_SHA": "a" * 40},
    )
    assert result["outputs"] == {"current": expected}
    assert bool(result["notices"]) is not expected


@pytest.mark.parametrize("number,sha", [("0", "a" * 40), ("710", "invalid"), ("710\nINJECTED=1", "a" * 40)])
def test_posting_rejects_invalid_report_identity(number, sha):
    script = workflow()["jobs"]["post-comment"]["steps"][-2]["with"]["script"]
    result = run_workflow_script(script, env={"PR_NUMBER": number, "REPORT_HEAD_SHA": sha})
    assert "Invalid coverage report" in result["error"]
    assert result["outputs"] == {}


def test_posting_does_not_write_if_head_lookup_fails():
    script = workflow()["jobs"]["post-comment"]["steps"][-2]["with"]["script"]
    result = run_workflow_script(
        script, apiError=True, env={"PR_NUMBER": "710", "REPORT_HEAD_SHA": "a" * 40},
    )
    assert result["error"] == "API failed"
    assert result["outputs"] == {}


@pytest.mark.parametrize("path", [
    ".github/workflows/pr-code-coverage.yml",
    ".github/workflows/coverage-tests.yml",
    ".github/scripts/coverage-report.py",
    ".github/scripts/wait-for-coverage.py",
    ".github/actions/post-coverage-comment/action.yml",
    ".github/coverage-requirements.txt",
    "scripts/test_coverage_report.py",
    ".pipeline/templates/validation-stages.yml",
    ".pipeline/scripts/evaluate-duplicate-pr.py",
])
def test_coverage_only_changes_trigger_lightweight_regressions(path):
    config = yaml.load(
        (ROOT / ".github" / "workflows" / "coverage-tests.yml").read_text(encoding="utf-8"),
        Loader=yaml.BaseLoader,
    )
    assert any(fnmatchcase(path, pattern) for pattern in config["on"]["pull_request"]["paths"])
    assert config["permissions"] == {"contents": "read"}
    job = config["jobs"]["coverage-tests"]
    assert job["steps"][0]["with"]["persist-credentials"] == "false"
    assert "python -m pytest scripts/test_coverage_report.py -q" in job["steps"][-1]["run"]


def ado_build(build_id=101, sha="a" * 40, status="inProgress", result=None, pr="710"):
    return {
        "id": build_id,
        "triggerInfo": {"pr.number": pr, "pr.sourceSha": sha},
        "status": status,
        "result": result,
    }


@pytest.fixture
def poll_clock(monkeypatch):
    clock = SimpleNamespace(now=0, sleeps=[])
    def sleep(seconds):
        clock.sleeps.append(seconds)
        clock.now += seconds
    monkeypatch.setattr(polling, "time", SimpleNamespace(monotonic=lambda: clock.now, sleep=sleep))
    monkeypatch.setattr(polling.random, "randint", lambda low, high: 300)
    monkeypatch.setattr(polling, "github_head", lambda pr, **kwargs: "a" * 40)
    return clock


def github_heads(monkeypatch, *heads):
    values = iter(heads)
    monkeypatch.setattr(polling, "github_head", lambda pr, **kwargs: next(values) * 40)


def queued_api(monkeypatch, responses):
    pending = list(responses)
    calls = []
    def get_json(url, *, deadline):
        calls.append(url)
        endpoint, values = pending.pop(0)
        parsed = urlparse(url)
        if endpoint in ("builds", "successful"):
            assert parsed.path.endswith("/builds")
            query = parse_qs(parsed.query)
            assert query["definitions"] == ["2267"]
            assert query["branchName"] == ["refs/pull/710/merge"]
            if endpoint == "successful":
                assert query["queryOrder"] == ["finishTimeDescending"]
                assert query["statusFilter"] == ["completed"]
                assert query["resultFilter"] == ["succeeded"]
                assert query["$top"] == ["200"]
            else:
                assert query["queryOrder"] == ["queueTimeDescending"]
        elif isinstance(endpoint, str) and endpoint.startswith("timeline:"):
            assert parsed.path.endswith(f"/builds/{endpoint.split(':')[1]}/timeline")
            return {"records": values}
        else:
            assert parsed.path.endswith(f"/builds/{endpoint}/artifacts")
        if isinstance(values, Exception):
            raise values
        return {"value": values}
    monkeypatch.setattr(polling, "get_json", get_json)
    return pending, calls


def test_poll_follows_replacement_build_after_new_commit(monkeypatch, poll_clock):
    github_heads(monkeypatch, "a", "b", "b", "b", "b")
    old = ado_build()
    canceled = ado_build(status="completed", result="canceled")
    new = ado_build(102, "b" * 40)
    complete = ado_build(102, "b" * 40, "completed", "succeeded")
    pending, calls = queued_api(monkeypatch, [
        ("builds", [old]), (101, []),
        ("builds", [canceled]),
        ("builds", [new, canceled]), (102, []),
        ("builds", [complete, canceled]), (102, [{"name": "CoberturaCoverage"}]),
        ("builds", [complete, canceled]),
    ])
    assert polling.wait_for_artifact("710") == (102, "b" * 40)
    assert not pending
    assert poll_clock.sleeps == [300, 10, 300]
    assert len([url for url in calls if "/101/artifacts" in url]) == 1


def test_poll_discards_artifact_if_build_changes_during_request(monkeypatch, poll_clock):
    github_heads(monkeypatch, "a", "b", "b")
    old = ado_build()
    new = ado_build(102, "b" * 40)
    pending, _ = queued_api(monkeypatch, [
        ("builds", [old]), (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [new, old]),
        ("builds", [new, old]), (102, [{"name": "CoberturaCoverage"}]),
        ("builds", [new, old]),
    ])
    assert polling.wait_for_artifact("710") == (102, "b" * 40)
    assert not pending


def test_poll_discards_artifact_if_build_is_canceled_during_request(monkeypatch, poll_clock):
    github_heads(monkeypatch, "a", "b", "b")
    old = ado_build()
    canceled = ado_build(status="completed", result="canceled")
    new = ado_build(102, "b" * 40)
    pending, _ = queued_api(monkeypatch, [
        ("builds", [old]), (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [canceled]),
        ("builds", [new]), (102, [{"name": "CoberturaCoverage"}]),
        ("builds", [new]),
    ])
    assert polling.wait_for_artifact("710") == (102, "b" * 40)
    assert not pending


def test_poll_does_not_fall_back_to_older_build(monkeypatch, poll_clock):
    new = ado_build(102)
    old = ado_build()
    pending, calls = queued_api(monkeypatch, [
        ("builds", [new, old]), (102, []),
        ("builds", [new, old]), (102, [{"name": "CoberturaCoverage"}]),
        ("builds", [new, old]),
    ])
    assert polling.wait_for_artifact("710") == (102, "a" * 40)
    assert not pending
    assert not any("/101/artifacts" in url for url in calls)


def test_poll_waits_through_missing_and_canceling_builds(monkeypatch, poll_clock):
    github_heads(monkeypatch, "a", "a", "b", "b")
    build = ado_build(102, "b" * 40)
    pending, _ = queued_api(monkeypatch, [
        ("builds", []),
        ("builds", [ado_build(status="cancelling")]),
        ("builds", [build]), (102, [{"name": "CoberturaCoverage"}]),
        ("builds", [build]),
    ])
    assert polling.wait_for_artifact("710") == (102, "b" * 40)
    assert not pending
    assert poll_clock.sleeps == [10, 10]


def test_poll_keeps_one_deadline_across_replacements(monkeypatch, poll_clock):
    pending, _ = queued_api(monkeypatch, [
        ("builds", [ado_build()]), (101, []),
        ("builds", [ado_build(102)]), (102, []),
    ])
    with pytest.raises(TimeoutError, match="latest build within 450 seconds"):
        polling.wait_for_artifact("710", timeout=450)
    assert poll_clock.sleeps == [300, 150]
    assert not pending


@pytest.mark.parametrize("builds", [[], [ado_build(status="completed", result="canceled")]])
def test_poll_times_out_without_replacement(monkeypatch, poll_clock, builds):
    pending, calls = queued_api(monkeypatch, [("builds", builds), ("builds", builds)])
    with pytest.raises(TimeoutError):
        polling.wait_for_artifact("710", timeout=15)
    assert poll_clock.sleeps == [10, 5]
    assert len(calls) == 2
    assert not pending


def test_poll_fails_explicitly_for_failed_latest_build(monkeypatch, poll_clock):
    pending, calls = queued_api(monkeypatch, [
        ("builds", [ado_build(status="completed", result="failed")]),
    ])
    with pytest.raises(RuntimeError, match="build 101 failed"):
        polling.wait_for_artifact("710")
    assert len(calls) == 1
    assert not pending


@pytest.mark.parametrize("field,value", [
    ("id", "1\nINJECTED=1"),
    ("id", True),
    ("id", 0),
    ("sha", "b" * 39),
    ("sha", "b" * 40 + "\nINJECTED=1"),
    ("sha", None),
])
def test_poll_validates_selected_build_outputs(monkeypatch, poll_clock, field, value):
    build = ado_build()
    if field == "sha":
        build["triggerInfo"]["pr.sourceSha"] = value
    else:
        build[field] = value
    pending, calls = queued_api(monkeypatch, [("builds", [build])])
    with pytest.raises(ValueError, match="Invalid"):
        polling.wait_for_artifact("710")
    assert len(calls) == 1
    assert not pending


def test_poll_filters_unrelated_pr_builds(monkeypatch, poll_clock):
    build = ado_build()
    pending, _ = queued_api(monkeypatch, [
        ("builds", [ado_build(999, pr="711"), build]), (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [build]),
    ])
    assert polling.wait_for_artifact("710") == (101, "a" * 40)
    assert not pending


def test_poll_rejects_duplicate_artifacts(monkeypatch, poll_clock):
    pending, _ = queued_api(monkeypatch, [
        ("builds", [ado_build()]),
        (101, [{"name": "CoberturaCoverage"}, {"name": "CoberturaCoverage"}]),
    ])
    with pytest.raises(ValueError, match="Multiple CoberturaCoverage artifacts"):
        polling.wait_for_artifact("710")
    assert not pending


@pytest.mark.parametrize("pr_number", ["0", "-1", "710&definitions=1", "710\nINJECTED=1"])
def test_poll_rejects_invalid_pr_number(monkeypatch, poll_clock, pr_number):
    pending, calls = queued_api(monkeypatch, [])
    with pytest.raises(ValueError, match="Invalid PR number"):
        polling.wait_for_artifact(pr_number)
    assert not calls


def test_poll_cli_outputs_matching_build_and_sha(monkeypatch, tmp_path):
    output = tmp_path / "output"
    monkeypatch.setattr(polling, "wait_for_artifact", lambda pr: (102, "b" * 40))
    monkeypatch.setattr("sys.argv", ["wait-for-coverage.py", "--pr-number", "710", "--output", str(output)])
    polling.main()
    assert output.read_text() == f"build_id=102\nhead_sha={'b' * 40}\n"


@pytest.mark.parametrize("key", ["pr.sourceSha", "pr.sourceCommitId", "pr.headSha"])
def test_poll_accepts_supported_sha_aliases(monkeypatch, poll_clock, key):
    build = ado_build()
    del build["triggerInfo"]["pr.sourceSha"]
    build["triggerInfo"][key] = "A" * 40
    pending, _ = queued_api(monkeypatch, [
        ("builds", [build]), (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [build]),
    ])
    assert polling.wait_for_artifact("710") == (101, "a" * 40)
    assert build["triggerInfo"]["pr.sourceSha"] == "a" * 40
    assert not pending


@pytest.mark.parametrize("old_result", ["succeeded", "failed"])
def test_poll_waits_for_current_head_before_replacement_is_queued(monkeypatch, poll_clock, old_result):
    github_heads(monkeypatch, "b", "b", "b", "b")
    old = ado_build(status="completed", result=old_result)
    new = ado_build(102, "b" * 40)
    pending, calls = queued_api(monkeypatch, [
        ("builds", [old]),
        ("builds", [old]),
        ("builds", [new, old]), (102, [{"name": "CoberturaCoverage"}]),
        ("builds", [new, old]),
    ])
    assert polling.wait_for_artifact("710") == (102, "b" * 40)
    assert poll_clock.sleeps == [10, 10]
    assert not any("/101/artifacts" in url for url in calls)
    assert not pending


def test_poll_refreshes_github_head_before_accepting_ready_artifact(monkeypatch, poll_clock):
    github_heads(monkeypatch, "a", "b", "b", "b", "b")
    old = ado_build(status="completed", result="succeeded")
    new = ado_build(102, "b" * 40)
    pending, calls = queued_api(monkeypatch, [
        ("builds", [old]), (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [old]),
        ("builds", [old]),
        ("builds", [new, old]), (102, [{"name": "CoberturaCoverage"}]),
        ("builds", [new, old]),
    ])
    assert polling.wait_for_artifact("710") == (102, "b" * 40)
    assert len([url for url in calls if "/101/artifacts" in url]) == 1
    assert not pending


def test_github_head_uses_read_only_job_token(monkeypatch):
    monkeypatch.setenv("GH_TOKEN", "fixture-token")
    def get_json(url, *, deadline, headers):
        assert url == "https://api.github.com/repos/microsoft/mssql-rs/pulls/710"
        assert headers["Authorization"] == "Bearer fixture-token"
        return {"head": {"sha": "B" * 40}}
    monkeypatch.setattr(polling, "get_json", get_json)
    assert polling.github_head("710", deadline=polling.time.monotonic() + 60) == "b" * 40


@pytest.mark.parametrize("sha", [None, "x" * 40, "a" * 40 + "\nINJECTED=1"])
def test_github_head_rejects_invalid_sha(monkeypatch, sha):
    monkeypatch.setenv("GH_TOKEN", "fixture-token")
    monkeypatch.setattr(polling, "get_json", lambda url, **kwargs: {"head": {"sha": sha}})
    with pytest.raises(ValueError, match="Invalid PR source SHA"):
        polling.github_head("710", deadline=polling.time.monotonic() + 60)


def duplicate_timeline(**changes):
    stages = {"EvaluateDuplicate": "succeeded", "Build": "skipped", "Coverage": "skipped"}
    stages.update(changes)
    return [{"type": "Stage", "identifier": name, "result": result} for name, result in stages.items()]


def test_poll_reuses_original_coverage_after_duplicate_finishes(monkeypatch, poll_clock):
    original = ado_build()
    completed = ado_build(status="completed", result="succeeded")
    duplicate = ado_build(102, status="completed", result="succeeded")
    pending, _ = queued_api(monkeypatch, [
        ("builds", [original]), (101, []),
        ("builds", [duplicate, completed]), (102, []),
        ("timeline:102", duplicate_timeline()),
        ("successful", [duplicate, completed]), ("builds", [duplicate]),
        (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [duplicate, completed]),
    ])
    assert polling.wait_for_artifact("710") == (101, "a" * 40)
    assert poll_clock.sleeps == [300]
    assert not pending


def test_duplicate_lookup_skips_chains_and_other_heads(monkeypatch, poll_clock):
    original = ado_build(status="completed", result="succeeded")
    duplicate = ado_build(103, status="completed", result="succeeded")
    earlier_duplicate = ado_build(102, status="completed", result="succeeded")
    other_head = ado_build(99, sha="b" * 40, status="completed", result="succeeded")
    other_pr = ado_build(98, status="completed", result="succeeded", pr="711")
    pending, calls = queued_api(monkeypatch, [
        ("builds", [duplicate]), (103, []),
        ("timeline:103", duplicate_timeline()),
        ("successful", [duplicate, other_head, other_pr, earlier_duplicate, original]),
        ("builds", [duplicate]), (102, []),
        ("builds", [duplicate]), (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [duplicate]),
    ])
    assert polling.wait_for_artifact("710") == (101, "a" * 40)
    assert not any("/99/artifacts" in url or "/98/artifacts" in url for url in calls)
    assert not pending


@pytest.mark.parametrize("changes", [
    {"EvaluateDuplicate": "skipped"},
    {"Build": "succeeded"},
    {"Coverage": "succeeded"},
    {"Coverage": None},
])
def test_poll_does_not_reuse_coverage_without_duplicate_stage_results(monkeypatch, poll_clock, changes):
    build = ado_build(status="completed", result="succeeded")
    pending, calls = queued_api(monkeypatch, [
        ("builds", [build]), (101, []),
        ("timeline:101", duplicate_timeline(**changes)),
    ])
    with pytest.raises(TimeoutError):
        polling.wait_for_artifact("710", timeout=30)
    assert len(calls) == 3
    assert not pending


def test_poll_follows_new_head_after_duplicate_coverage_lookup(monkeypatch, poll_clock):
    github_heads(monkeypatch, "a", "a", "b", "b", "b")
    original = ado_build(status="completed", result="succeeded")
    duplicate = ado_build(102, status="completed", result="succeeded")
    replacement = ado_build(103, sha="b" * 40)
    pending, _ = queued_api(monkeypatch, [
        ("builds", [duplicate]), (102, []),
        ("timeline:102", duplicate_timeline()),
        ("successful", [original]), ("builds", [duplicate]),
        (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [duplicate]),
        ("builds", [replacement, duplicate]), (103, [{"name": "CoberturaCoverage"}]),
        ("builds", [replacement, duplicate]),
    ])
    assert polling.wait_for_artifact("710") == (103, "b" * 40)
    assert not pending


def test_duplicate_lookup_without_available_coverage_stays_bounded(monkeypatch, poll_clock):
    duplicate = ado_build(102, status="completed", result="succeeded")
    original = ado_build(status="completed", result="succeeded")
    pending, _ = queued_api(monkeypatch, [
        ("builds", [duplicate]), (102, []),
        ("timeline:102", duplicate_timeline()),
        ("successful", [duplicate, original]), ("builds", [duplicate]), (101, []),
    ])
    with pytest.raises(TimeoutError):
        polling.wait_for_artifact("710", timeout=30)
    assert poll_clock.sleeps == [30]
    assert not pending


@pytest.mark.parametrize("change", ["head", "build"])
def test_history_scan_follows_replacement_before_next_artifact(monkeypatch, poll_clock, change):
    duplicate = ado_build(103, status="completed", result="succeeded")
    history = [
        ado_build(102, status="completed", result="succeeded"),
        ado_build(101, status="completed", result="succeeded"),
    ]
    replacement_sha = "b" * 40 if change == "head" else "a" * 40
    replacement = ado_build(104, sha=replacement_sha)
    if change == "head":
        github_heads(monkeypatch, "a", "a", "b", "b", "b")
    responses = [
        ("builds", [duplicate]), (103, []),
        ("timeline:103", duplicate_timeline()),
        ("successful", history),
        ("builds", [duplicate]), (102, []),
    ]
    if change == "build":
        responses.append(("builds", [replacement]))
    responses.extend([
        ("builds", [replacement]), (104, [{"name": "CoberturaCoverage"}]),
        ("builds", [replacement]),
    ])
    pending, calls = queued_api(monkeypatch, responses)
    has_coverage = polling.has_coverage
    def slow_first_artifact(build_id, *, deadline):
        result = has_coverage(build_id, deadline=deadline)
        if build_id == 102:
            poll_clock.now += 40
        return result
    monkeypatch.setattr(polling, "has_coverage", slow_first_artifact)
    assert polling.wait_for_artifact("710", timeout=60) == (104, replacement_sha)
    assert not any("/101/artifacts" in url for url in calls)
    assert poll_clock.now == 40
    assert not poll_clock.sleeps
    assert not pending


@pytest.mark.parametrize("status", [408, 429, 500, 502, 503, 504])
def test_poll_recovers_from_transient_artifact_errors(monkeypatch, poll_clock, status):
    old = ado_build()
    new = ado_build(102)
    error = HTTPError("https://dev.azure.com/artifacts", status, "temporary", {}, None)
    pending, _ = queued_api(monkeypatch, [
        ("builds", [old]), (101, error),
        ("builds", [new]), (102, [{"name": "CoberturaCoverage"}]),
        ("builds", [new]),
    ])
    assert polling.wait_for_artifact("710") == (102, "a" * 40)
    assert poll_clock.sleeps == [10]
    assert not pending


def test_poll_retry_backoff_does_not_reset_deadline(monkeypatch, poll_clock):
    errors = [
        ("builds", HTTPError("https://dev.azure.com/builds", 503, "temporary", {}, None))
        for _ in range(8)
    ]
    pending, _ = queued_api(monkeypatch, errors)
    with pytest.raises(TimeoutError, match="within 1000 seconds"):
        polling.wait_for_artifact("710", timeout=1000)
    assert poll_clock.sleeps == [10, 20, 40, 80, 160, 300, 300, 90]
    assert not pending


def test_poll_resets_backoff_after_successful_api_calls(monkeypatch, poll_clock):
    build = ado_build()
    error = HTTPError("https://dev.azure.com/artifacts", 503, "temporary", {}, None)
    pending, _ = queued_api(monkeypatch, [
        ("builds", [build]), (101, error),
        ("builds", [build]), (101, []),
        ("builds", [build]), (101, error),
        ("builds", [build]), (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [build]),
    ])
    assert polling.wait_for_artifact("710") == (101, "a" * 40)
    assert poll_clock.sleeps == [10, 300, 10]
    assert not pending


def test_poll_recovers_from_request_timeout(monkeypatch, poll_clock):
    build = ado_build()
    pending, _ = queued_api(monkeypatch, [
        ("builds", TimeoutError("request timed out")),
        ("builds", [build]), (101, [{"name": "CoberturaCoverage"}]),
        ("builds", [build]),
    ])
    assert polling.wait_for_artifact("710") == (101, "a" * 40)
    assert poll_clock.sleeps == [10]
    assert not pending


@pytest.mark.parametrize("status", [400, 401, 403, 404, 422])
def test_poll_does_not_retry_permanent_http_errors(monkeypatch, poll_clock, status):
    error = HTTPError("https://dev.azure.com/builds", status, "permanent", {}, None)
    pending, _ = queued_api(monkeypatch, [("builds", error)])
    with pytest.raises(HTTPError):
        polling.wait_for_artifact("710")
    assert not poll_clock.sleeps
    assert not pending


def test_poll_does_not_retry_invalid_json(monkeypatch, poll_clock):
    error = json.JSONDecodeError("invalid", "", 0)
    pending, _ = queued_api(monkeypatch, [("builds", error)])
    with pytest.raises(json.JSONDecodeError):
        polling.wait_for_artifact("710")
    assert not poll_clock.sleeps
    assert not pending


def curl_response(payload, status=200):
    return subprocess.CompletedProcess(
        [], 0 if status < 400 else 22, f"{json.dumps(payload)}\n{status}", ""
    )


@pytest.mark.parametrize("timeout_kind", ["curl", "subprocess"])
def test_poll_retries_transfer_timeout(monkeypatch, poll_clock, timeout_kind):
    build = ado_build()
    responses = [
        subprocess.CompletedProcess([], 28, "", "connection timed out")
        if timeout_kind == "curl" else subprocess.TimeoutExpired("curl", 60),
        {"value": [build]},
        {"value": [{"name": "CoberturaCoverage"}]},
        {"value": [build]},
    ]
    def run(command, **kwargs):
        response = responses.pop(0)
        if isinstance(response, Exception):
            raise response
        return response if isinstance(response, subprocess.CompletedProcess) else curl_response(response)
    monkeypatch.setattr(polling.subprocess, "run", run)
    assert polling.wait_for_artifact("710") == (101, "a" * 40)
    assert poll_clock.sleeps == [10]
    assert not responses


def test_poll_does_not_retry_other_url_errors(monkeypatch, poll_clock):
    def run(command, **kwargs):
        return subprocess.CompletedProcess([], 60, "", "certificate verification failed")
    monkeypatch.setattr(polling.subprocess, "run", run)
    with pytest.raises(URLError):
        polling.wait_for_artifact("710")
    assert not poll_clock.sleeps


def test_request_timeout_is_capped_by_remaining_budget(monkeypatch, poll_clock):
    poll_clock.now = 58
    def run(command, **kwargs):
        assert kwargs["timeout"] == 2
        assert command[command.index("--max-time") + 1] == "2"
        return curl_response({})
    monkeypatch.setattr(polling.subprocess, "run", run)
    assert polling.get_json("https://dev.azure.com/builds", deadline=60) == {}


def test_expired_deadline_prevents_new_requests(monkeypatch, poll_clock):
    poll_clock.now = 60
    def run(*args, **kwargs):
        pytest.fail("No request may start after the polling deadline")
    monkeypatch.setattr(polling.subprocess, "run", run)
    with pytest.raises(polling.PollingDeadlineExceeded):
        polling.get_json("https://dev.azure.com/builds", deadline=60)


def test_response_arriving_after_deadline_is_not_accepted(monkeypatch, poll_clock):
    def run(command, **kwargs):
        poll_clock.now += 61
        return curl_response({})
    monkeypatch.setattr(polling.subprocess, "run", run)
    with pytest.raises(polling.PollingDeadlineExceeded):
        polling.get_json("https://dev.azure.com/builds", deadline=60)


def test_duplicate_history_requests_share_the_polling_deadline(monkeypatch, poll_clock):
    duplicate = ado_build(200, status="completed", result="succeeded")
    history = [ado_build(build_id, status="completed", result="succeeded") for build_id in range(199, 0, -1)]
    scanned = []
    def run(command, **kwargs):
        parsed = urlparse(command[-1])
        if parsed.path.endswith("/builds"):
            builds = history if "resultFilter" in parse_qs(parsed.query) else [duplicate]
            return curl_response({"value": builds})
        if parsed.path.endswith("/timeline"):
            return curl_response({"records": duplicate_timeline()})
        build_id = int(parsed.path.split("/")[-2])
        if build_id != 200:
            scanned.append((build_id, kwargs["timeout"]))
            poll_clock.now += 40
        return curl_response({"value": []})
    monkeypatch.setattr(polling.subprocess, "run", run)
    with pytest.raises(polling.PollingDeadlineExceeded):
        polling.wait_for_artifact("710", timeout=120)
    assert scanned == [(199, 60), (198, 60), (197, 40)]
    assert poll_clock.now == 120
    assert not poll_clock.sleeps


@pytest.mark.parametrize("status", [429, 503, 401, 404])
def test_request_preserves_http_error_status(monkeypatch, poll_clock, status):
    monkeypatch.setattr(polling.subprocess, "run", lambda *args, **kwargs: curl_response({}, status))
    with pytest.raises(HTTPError) as error:
        polling.get_json("https://dev.azure.com/builds", deadline=60)
    assert error.value.code == status


def test_request_keeps_authorization_out_of_command_arguments(monkeypatch, poll_clock):
    def run(command, **kwargs):
        assert "fixture-token" not in " ".join(command)
        assert kwargs["input"] == "Authorization: Bearer fixture-token\n"
        assert command[command.index("--header") + 1] == "@-"
        return curl_response({})
    monkeypatch.setattr(polling.subprocess, "run", run)
    polling.get_json(
        "https://api.github.com/repos/microsoft/mssql-rs/pulls/710",
        deadline=60, headers={"Authorization": "Bearer fixture-token"},
    )


def test_request_rejects_header_newlines(monkeypatch, poll_clock):
    with pytest.raises(ValueError, match="Invalid HTTP header"):
        polling.get_json("https://dev.azure.com/builds", deadline=60, headers={"Accept": "json\nInjected: value"})


def test_request_interrupts_trickling_response():
    received = threading.Event()
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            body = b'{"value":[]}'
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            received.set()
            try:
                for byte in body:
                    self.wfile.write(bytes([byte]))
                    self.wfile.flush()
                    time.sleep(0.2)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def log_message(self, *args):
            pass

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            start = time.monotonic()
            with pytest.raises(TimeoutError):
                polling.get_json(f"http://127.0.0.1:{server.server_port}/", deadline=start + 0.3)
            elapsed = time.monotonic() - start
            assert received.is_set()
            assert elapsed < 1.0
        finally:
            server.shutdown()
            thread.join()

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Exercise the ADO sync pipeline without contacting Git or Azure services."""

import os
import shutil
import subprocess
from pathlib import Path

import pytest
import yaml

_ROOT = Path(__file__).parents[1]
_BASH = shutil.which("bash")
_SYNC_BRANCH = "sync/github-main-to-stable"
_GH_SHA = "a" * 40
_TARGET_SHA = "b" * 40


@pytest.fixture
def pipeline():
    return yaml.safe_load(
        (_ROOT / ".pipeline" / "sync-github-main-to-stable.yml").read_text(
            encoding="utf-8"
        )
    )


@pytest.fixture
def sync_step(pipeline):
    return next(step for step in pipeline["steps"] if "script" in step)


def test_sync_destination_is_not_pipeline_configuration(pipeline, sync_step):
    assert not pipeline.get("parameters")
    assert "SYNC_BRANCH" not in sync_step["env"]
    assert f"readonly SYNC_BRANCH='{_SYNC_BRANCH}'" in sync_step["script"]
    assert "$(SYNC_BRANCH)" not in sync_step["script"]
    assert "${{ parameters." not in sync_step["script"]
    assert sync_step["env"]["TARGET_BRANCH"] == "stable"
    ado = next(
        repo for repo in pipeline["resources"]["repositories"]
        if repo["repository"] == "ado"
    )
    assert ado["ref"] == "refs/heads/stable"


def test_sync_tests_run_in_shared_validation():
    pipeline = yaml.safe_load(
        (_ROOT / ".pipeline" / "templates" / "validation-stages.yml").read_text(
            encoding="utf-8"
        )
    )
    build = next(stage for stage in pipeline["stages"] if stage["stage"] == "Build")
    windows = next(job for job in build["jobs"] if job.get("job") == "Build_Windows")
    step = next(
        step for step in windows["steps"]
        if step.get("displayName") == "Unit test packaging scripts and pipeline configuration"
    )
    command = next(line for line in step["pwsh"].splitlines() if "python -m pytest" in line)
    assert "scripts/test_sync_pipeline.py" in command.split()
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)

    linux = next(job for job in build["jobs"] if job.get("job") == "Build_Linux")
    step = next(
        step for step in linux["steps"]
        if step.get("displayName") == "Test ADO sync pipeline"
    )
    script = step["bash"]
    assert "set -euo pipefail" in script
    assert 'assert shutil.which("bash")' in script
    assert "python -m pytest scripts/test_sync_pipeline.py -q -rs" in script
    assert script.index('assert shutil.which("bash")') < script.index("python -m pytest")
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)


@pytest.fixture
def run_sync(tmp_path, sync_step):
    if _BASH is None:
        pytest.skip("Bash is required")
    (tmp_path / "s" / "ado").mkdir(parents=True)
    (tmp_path / "s" / "gh").mkdir()
    script = sync_step["script"].replace("$(Agent.BuildDirectory)", "$PWD")
    script = script.replace(
        "$(System.TeamFoundationCollectionUri)", "https://dev.azure.com/test"
    ).replace("$(System.TeamProject)", "test-project")
    mocks = r"""
TEST_CALL_LOG="$PWD/calls.log"
record() { printf '%s\0' "$@" >> "$TEST_CALL_LOG"; printf '\0' >> "$TEST_CALL_LOG"; }
git() {
    record git "$@"
    case "$1" in
        fetch)
            if [[ "$2" == origin ]]; then FETCH_SHA="$MOCK_TARGET_SHA"
            else FETCH_SHA="$MOCK_GH_SHA"; fi ;;
        rev-parse) printf '%s\n' "$FETCH_SHA" ;;
        diff) return "$MOCK_DIFF_STATUS" ;;
        push) return "$MOCK_PUSH_STATUS" ;;
        *) echo "Unexpected git command" >&2; return 90 ;;
    esac
}
az() {
    record az "$@"
    case "$1 $2 $3" in
        "repos pr list")
            printf '%s\n' "$MOCK_PR_ID"
            return "$MOCK_LIST_STATUS" ;;
        "repos pr create") ;;
        *) echo "Unexpected az command" >&2; return 90 ;;
    esac
}
"""
    script_path = tmp_path / "run-sync.sh"
    script_path.write_text(mocks + script, encoding="utf-8", newline="\n")

    def run(*, override="", diff_status=1, pr_id="", push_status=0, list_status=0):
        env = {
            **os.environ,
            **sync_step["env"],
            "SYNC_BRANCH": override,
            "SYNCBRANCH": override,
            "MOCK_GH_SHA": _GH_SHA,
            "MOCK_TARGET_SHA": _TARGET_SHA,
            "MOCK_DIFF_STATUS": str(diff_status),
            "MOCK_PR_ID": pr_id,
            "MOCK_PUSH_STATUS": str(push_status),
            "MOCK_LIST_STATUS": str(list_status),
        }
        result = subprocess.run(
            [_BASH, script_path.name], cwd=tmp_path,
            env=env, capture_output=True, text=True, timeout=15,
        )
        call_log = tmp_path / "calls.log"
        assert call_log.exists(), result.stdout + result.stderr
        calls = [
            entry.decode("utf-8").split("\0")
            for entry in call_log.read_bytes().split(b"\0\0")[:-1]
        ]
        return result, calls

    return run


def test_sync_script_uses_lf_newlines(run_sync, tmp_path):
    result, _ = run_sync(diff_status=0)
    assert result.returncode == 0, result.stdout + result.stderr
    script = (tmp_path / "run-sync.sh").read_bytes()
    assert b"\n" in script
    assert b"\r" not in script


@pytest.mark.parametrize(
    "override",
    [
        "", _SYNC_BRANCH, "main", "stable", "sync/another-branch",
        "--mirror", 'main";\necho injected',
    ],
)
@pytest.mark.parametrize("flow", ["no-change", "existing-pr", "new-pr"])
def test_sync_flow_ignores_inherited_destination(run_sync, override, flow):
    result, calls = run_sync(
        override=override,
        diff_status=0 if flow == "no-change" else 1,
        pr_id="123" if flow == "existing-pr" else "",
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert calls[0][0:2] == ["git", "fetch"]
    assert calls[0][2].replace("\\", "/").endswith("/s/gh")
    assert calls[0][3:] == ["HEAD"]
    assert calls[1:5] == [
        ["git", "rev-parse", "FETCH_HEAD"],
        ["git", "fetch", "origin", "stable"],
        ["git", "rev-parse", "FETCH_HEAD"],
        ["git", "diff", "--quiet", f"{_TARGET_SHA}..{_GH_SHA}"],
    ]
    if flow == "no-change":
        assert len(calls) == 5
        assert "No changes" in result.stdout
        return

    assert calls[5] == [
        "git", "push", "origin", "--force", f"{_GH_SHA}:refs/heads/{_SYNC_BRANCH}"
    ]
    common_args = [
        "--organization", "https://dev.azure.com/test",
        "--project", "test-project",
        "--repository", "mssql-rs",
        "--source-branch", _SYNC_BRANCH,
        "--target-branch", "stable",
    ]
    assert calls[6] == [
        "az", "repos", "pr", "list", *common_args,
        "--status", "active", "--query", "[0].pullRequestId", "--output", "tsv",
    ]
    if flow == "existing-pr":
        assert len(calls) == 7
        assert "Active PR #123" in result.stdout
    else:
        assert len(calls) == 8
        assert calls[7] == [
            "az", "repos", "pr", "create", *common_args,
            "--title", "Sync GitHub main to stable", "--description",
            "Sync of GitHub microsoft/mssql-rs main into the stable branch.\n\n"
            "Before merging, ensure CI has had a good run on GitHub main.",
        ]
        assert "No active PR found" in result.stdout


@pytest.mark.parametrize(
    ("failure", "expected_calls"),
    [({"push_status": 23}, 6), ({"list_status": 24}, 7)],
)
def test_sync_failure_does_not_create_pr(run_sync, failure, expected_calls):
    result, calls = run_sync(**failure)
    assert result.returncode == next(iter(failure.values())), result.stdout + result.stderr
    assert len(calls) == expected_calls
    assert not any(call[:4] == ["az", "repos", "pr", "create"] for call in calls)

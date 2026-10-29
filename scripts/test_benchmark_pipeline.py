# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Benchmark ref handling, executed with mocked network and build commands."""

import os
import shlex
import shutil
import subprocess
from pathlib import Path

import pytest
import yaml

_ROOT = Path(__file__).parents[1]
_BASH = shutil.which("bash")
_PR_SHA = "a" * 40
_BASELINE_SHA = "b" * 40
_PUBLIC_URL = "https://github.com/microsoft/mssql-rs.git"


@pytest.fixture
def steps():
    pipeline = yaml.safe_load(
        (_ROOT / ".pipeline" / "benchmark-pipeline.yml").read_text(encoding="utf-8")
    )
    return pipeline["stages"][0]["jobs"][0]["steps"]


def named(steps, display_name):
    return next(step for step in steps if step.get("displayName") == display_name)


@pytest.fixture
def run_script(tmp_path):
    if _BASH is None:
        if os.environ.get("REQUIRE_BASH_TESTS") == "1":
            pytest.fail("Bash is required for enforced benchmark regression tests")
        pytest.skip("Bash is required")

    def run(script, **env):
        # Only check-ref-format uses real Git; no fetch, checkout, or build can escape.
        mocks = r"""
git() {
  printf 'git' >> commands.log
  printf '\t%s' "$@" >> commands.log
  printf '\n' >> commands.log
  case "$1" in
    check-ref-format) command git "$@" ;;
    rev-parse)
      case "$3" in
        'HEAD^{commit}') printf '%s\n' "$MOCK_PR_SHA" ;;
        'FETCH_HEAD^{commit}') printf '%s\n' "$MOCK_BASELINE_SHA" ;;
        *) return 97 ;;
      esac ;;
    fetch)
      printf 'prompt\t%s\n' "$GIT_TERMINAL_PROMPT" >> commands.log
      return "$MOCK_FETCH_STATUS" ;;
    checkout) return "$MOCK_CHECKOUT_STATUS" ;;
    *) return 98 ;;
  esac
}
cargo() {
  printf 'cargo' >> commands.log
  printf '\t%s' "$@" >> commands.log
  printf '\n' >> commands.log
}
"""
        inputs = {
            "BUILD_REASON": "Manual",
            "PR_TARGET_REF": "$(System.PullRequest.TargetBranch)",
            "MANUAL_BASELINE_REF": "main",
            "MOCK_PR_SHA": _PR_SHA,
            "MOCK_BASELINE_SHA": _BASELINE_SHA,
            "MOCK_FETCH_STATUS": "0",
            "MOCK_CHECKOUT_STATUS": "0",
            **env,
        }
        # WSL launchers do not forward arbitrary Windows environment variables.
        exports = ""
        for key, value in inputs.items():
            # Git Bash's igncr option can discard literal CRs even inside quotes.
            quoted = shlex.quote(value).replace("\r", "'$'\\r''")
            exports += f"export {key}={quoted}\n"
        script_path = tmp_path / "run-step.sh"
        script_path.write_text(exports + mocks + script, encoding="utf-8", newline="\n")
        result = subprocess.run(
            [_BASH, script_path.name], cwd=tmp_path,
            capture_output=True, text=True, timeout=30,
        )
        log = tmp_path / "commands.log"
        commands = [line.split("\t") for line in log.read_text().splitlines()] if log.exists() else []
        assert not (tmp_path / "injected").exists()
        return result, commands

    return run


def test_bash_script_uses_lf_newlines(run_script, tmp_path):
    result, _ = run_script("printf 'first\\n'\nprintf 'second\\n'\n")
    assert result.returncode == 0, result.stdout + result.stderr
    assert result.stdout == "first\nsecond\n"
    assert b"\r" not in (tmp_path / "run-step.sh").read_bytes()


def test_fixture_inputs_survive_launcher_environment_loss(run_script, tmp_path, monkeypatch):
    original_run = subprocess.run

    def drop_inputs(*args, **kwargs):
        kwargs["env"] = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("BUILD_", "PR_", "MANUAL_", "MOCK_"))
        }
        return original_run(*args, **kwargs)

    monkeypatch.setattr(subprocess, "run", drop_inputs)
    payload = "'\n$(touch injected); `touch injected`\r\n"
    result, _ = run_script(
        'set -eu\nprintf "%s" "$MANUAL_BASELINE_REF" > input.txt\n'
        'test "$BUILD_REASON" = Manual\ntest "$MOCK_FETCH_STATUS" = 7\n',
        MANUAL_BASELINE_REF=payload, MOCK_FETCH_STATUS="7",
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert (tmp_path / "input.txt").read_bytes() == payload.encode("utf-8")


def test_checkout_and_fetch_precede_repository_execution(steps):
    assert steps[0] == {
        "checkout": "self", "fetchDepth": 0, "persistCredentials": False,
    }
    resolve = named(steps, "Resolve and fetch benchmark refs")
    assert steps[1] is resolve
    assert resolve.get("condition", "succeeded()") == "succeeded()"
    assert not resolve.get("continueOnError", False)
    assert resolve["env"] == {
        "BUILD_REASON": "$(Build.Reason)",
        "PR_TARGET_REF": "$(System.PullRequest.TargetBranch)",
        "MANUAL_BASELINE_REF": "${{ parameters.baselineBranch }}",
    }
    for step in steps:
        script = step.get("script", "")
        assert "${{" not in script
        for macro in ("Build.Reason", "System.PullRequest.TargetBranch", "BASELINE_BRANCH", "BASELINE_SHA", "PR_SHA"):
            assert f"$({macro})" not in script
        if step is not resolve:
            assert "git fetch" not in script


@pytest.mark.parametrize("required", ["0", "1"])
def test_missing_bash_policy(tmp_path, monkeypatch, required):
    monkeypatch.setitem(run_script.__wrapped__.__globals__, "_BASH", None)
    monkeypatch.setenv("REQUIRE_BASH_TESTS", required)
    expected = pytest.fail.Exception if required == "1" else pytest.skip.Exception
    with pytest.raises((pytest.fail.Exception, pytest.skip.Exception),
                       match="Bash is required") as raised:
        run_script.__wrapped__(tmp_path)
    assert isinstance(raised.value, expected)


@pytest.mark.parametrize(
    ("reason", "pr_ref", "manual_ref", "expected"),
    [
        ("PullRequest", "refs/heads/main", "$(touch injected)", "main"),
        ("PullRequest", "refs/heads/release/1.0", "ignored", "release/1.0"),
        ("PullRequest", "main", "ignored", "main"),
        ("Manual", "$(touch injected)", "main", "main"),
        ("Manual", "", "feature/benchmark-fix", "feature/benchmark-fix"),
        ("Manual", "", "_benchmark", "_benchmark"),
        ("Manual", "", "refs/heads/main", "refs/heads/main"),
        ("Manual", "", "v1.2.3", "v1.2.3"),
        ("Manual", "", "refs/tags/v1.2.3", "refs/tags/v1.2.3"),
        ("Manual", "", _BASELINE_SHA, _BASELINE_SHA),
    ],
)
def test_supported_baselines(steps, run_script, reason, pr_ref, manual_ref, expected):
    result, commands = run_script(
        named(steps, "Resolve and fetch benchmark refs")["script"],
        BUILD_REASON=reason, PR_TARGET_REF=pr_ref, MANUAL_BASELINE_REF=manual_ref,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert commands == [
        ["git", "check-ref-format", "--allow-onelevel", expected],
        ["git", "rev-parse", "--verify", "HEAD^{commit}"],
        ["git", "fetch", "--no-tags", "--", _PUBLIC_URL, expected],
        ["prompt", "0"],
        ["git", "rev-parse", "--verify", "FETCH_HEAD^{commit}"],
    ]
    for name, value in (
        ("PR_SHA", _PR_SHA), ("BASELINE_BRANCH", expected), ("BASELINE_SHA", _BASELINE_SHA),
    ):
        assert f"##vso[task.setvariable variable={name};isReadOnly=true]{value}" in result.stdout


@pytest.mark.parametrize("reason", ["Manual", "PullRequest"])
@pytest.mark.parametrize(
    "ref",
    [
        "", "-main", "--upload-pack=touch injected", "main:refs/heads/other",
        "+refs/heads/main", "^refs/heads/main", "main~1", "HEAD^{commit}", "@{-1}",
        "main..other", "refs/heads/bad.lock", "refs//heads/main", "main/", "main.",
        ".hidden", "main other", "main\tother", "main\n##vso[task.setvariable variable=X]bad",
        "main\rdata", 'main"; touch injected; #', "$(touch injected)", "`touch injected`",
        "main;touch injected", "main|touch injected", "main&touch injected",
        "refs/heads/*", "https://example.invalid/repo", "main\\other",
        "main%0Ainjected", "refs/heads/--upload-pack=touch injected",
    ],
)
def test_invalid_baselines_fail_before_fetch_or_logging(steps, run_script, reason, ref):
    result, commands = run_script(
        named(steps, "Resolve and fetch benchmark refs")["script"],
        BUILD_REASON=reason, PR_TARGET_REF=ref, MANUAL_BASELINE_REF=ref,
    )
    assert result.returncode != 0
    assert "Invalid benchmark baseline ref" in result.stdout
    assert "task.setvariable" not in result.stdout
    assert "Baseline branch:" not in result.stdout
    assert all(command[:2] == ["git", "check-ref-format"] for command in commands)


def test_failed_fetch_does_not_use_stale_fetch_head(steps, run_script):
    result, commands = run_script(
        named(steps, "Resolve and fetch benchmark refs")["script"], MOCK_FETCH_STATUS="1",
    )
    assert result.returncode != 0
    assert "task.setvariable" not in result.stdout
    assert ["git", "rev-parse", "--verify", "FETCH_HEAD^{commit}"] not in commands


@pytest.mark.parametrize("variable", ["MOCK_PR_SHA", "MOCK_BASELINE_SHA"])
def test_invalid_resolved_commit_is_not_published(steps, run_script, variable):
    result, _ = run_script(
        named(steps, "Resolve and fetch benchmark refs")["script"],
        **{variable: "bad\n##vso[task.setvariable variable=X]bad"},
    )
    assert result.returncode != 0
    assert "Invalid benchmark commit SHA" in result.stdout
    assert "task.setvariable" not in result.stdout


@pytest.mark.parametrize(
    ("display_name", "variable", "sha", "baseline"),
    [
        ("Baseline benchmarks ($(BASELINE_BRANCH))", "BASELINE_SHA", _BASELINE_SHA, "base"),
        ("PR benchmarks", "PR_SHA", _PR_SHA, "pr"),
    ],
)
def test_benchmarks_checkout_pinned_commits(steps, run_script, display_name, variable, sha, baseline):
    step = named(steps, display_name)
    assert step["env"][variable] == f"$({variable})"
    result, commands = run_script(step["script"], **{variable: sha})
    assert result.returncode == 0, result.stdout + result.stderr
    assert commands == [
        ["git", "checkout", "--detach", sha],
        ["cargo", "bench", "--package", "mssql-tds", "--bench", "perf", "--", "--save-baseline", baseline],
    ]
    assert "critcmp base pr" in named(steps, "Compare results")["script"]
    publish = named(steps, "Publish benchmark results")
    assert publish["inputs"]["artifactName"] == "benchmark-results"


@pytest.mark.parametrize(
    "display_name", ["Baseline benchmarks ($(BASELINE_BRANCH))", "PR benchmarks"],
)
def test_failed_checkout_does_not_run_benchmarks(steps, run_script, display_name):
    result, commands = run_script(
        named(steps, display_name)["script"], MOCK_CHECKOUT_STATUS="1",
        PR_SHA=_PR_SHA, BASELINE_SHA=_BASELINE_SHA,
    )
    assert result.returncode != 0
    assert len(commands) == 1
    assert commands[0][:3] == ["git", "checkout", "--detach"]


def test_benchmark_tests_run_in_shared_validation():
    stages = yaml.safe_load(
        (_ROOT / ".pipeline" / "templates" / "validation-stages.yml").read_text(encoding="utf-8")
    )["stages"]
    build = next(stage for stage in stages if stage["stage"] == "Build")
    windows = next(job for job in build["jobs"] if job.get("job") == "Build_Windows")
    step = named(windows["steps"], "Unit test packaging scripts and pipeline configuration")
    command = next(line for line in step["pwsh"].splitlines() if "python -m pytest" in line)
    assert "scripts/test_benchmark_pipeline.py" in command.split()
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)

    linux = next(job for job in build["jobs"] if job.get("job") == "Build_Linux")
    step = named(linux["steps"], "Test benchmark pipeline scripts")
    assert "set -euo pipefail" in step["bash"]
    assert "command -v bash" in step["bash"]
    assert "command -v git" in step["bash"]
    assert "-m pytest scripts/test_benchmark_pipeline.py -q -rs" in step["bash"]
    assert step["env"]["REQUIRE_BASH_TESTS"] == "1"
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)

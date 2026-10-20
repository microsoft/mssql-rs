# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Cargo credential forwarding contracts; no Docker daemon or feed access needed."""

import os
import shlex
import shutil
import subprocess
import uuid
from pathlib import Path

import pytest
import yaml

_ROOT = Path(__file__).parents[1]
_TEMPLATES = _ROOT / ".pipeline" / "templates"
_BASH = shutil.which("bash")
_REGISTRIES = ("MSSQL_RS_PUBLIC", "MSSQL_RS")
_TOKENS = tuple(f"CARGO_REGISTRIES_{name}_TOKEN" for name in _REGISTRIES)
_PROVIDERS = tuple(f"CARGO_REGISTRIES_{name}_CREDENTIAL_PROVIDER" for name in _REGISTRIES)
_NON_PR_ENV = "${{ if ne(variables['Build.Reason'], 'PullRequest') }}"
_ARGS = ("--rm", "-v", "workspace with spaces:/workspace", "test-image", "cargo", "check")


@pytest.fixture
def run_wrapper(tmp_path):
    if _BASH is None:
        if os.name == "nt":
            pytest.skip("Bash execution is enforced by the Linux pipeline job")
        pytest.fail("Bash is required for the Cargo credential regression tests")
    shutil.copyfile(
        _ROOT / ".pipeline" / "scripts" / "docker-cargo-run.sh",
        tmp_path / "docker-cargo-run.sh",
    )
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "docker").write_text(
        """#!/bin/bash
set -euo pipefail
for registry in MSSQL_RS_PUBLIC MSSQL_RS; do
  for suffix in TOKEN CREDENTIAL_PROVIDER; do
    name="CARGO_REGISTRIES_${registry}_${suffix}"
    if [[ -v "$name" ]]; then
      printf '%s\\0' "$1" "$name" "${!name}" >> docker-env
    fi
  done
done
case "$1" in
  info) printf '/mock-docker-root\\n' ;;
  run)
    printf '%s\\0' "$@" > docker-args
    exit "${DOCKER_RUN_EXIT:-0}"
    ;;
  *) echo "Unexpected Docker invocation" >&2; exit 99 ;;
esac
""",
        encoding="utf-8",
        newline="\n",
    )
    (bin_dir / "df").write_text(
        "#!/bin/bash\nprintf 'Filesystem Blocks Used Available Use%% Mounted\\n"
        "mock 100 10 90 10%% /\\n'\n",
        encoding="utf-8",
        newline="\n",
    )

    def run(values):
        env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("CARGO_") and key not in ("BUILD_REASON", "DOCKER_RUN_EXIT")
        }
        # Export inside Bash so WSL receives the same inputs as Git Bash/Linux.
        setup = "unset " + " ".join((*_TOKENS, *_PROVIDERS, "BUILD_REASON", "DOCKER_RUN_EXIT")) + "; "
        setup += "".join(
            f"export {name}={shlex.quote(value)}; " for name, value in values.items()
        )
        # Keep quoting out of the Windows WSL launcher's command-line parser.
        (tmp_path / "invoke.sh").write_text(
            setup + 'chmod +x bin/docker bin/df; export PATH="$PWD/bin:$PATH"; '
            "exec bash ./docker-cargo-run.sh " + shlex.join(_ARGS) + "\n",
            encoding="utf-8",
            newline="\n",
        )
        result = subprocess.run(
            [_BASH, "-c", "bash ./invoke.sh"],
            cwd=tmp_path,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert (tmp_path / "docker-args").exists(), result.stdout + result.stderr
        args = (tmp_path / "docker-args").read_bytes().decode().split("\0")[:-1]
        environment = tmp_path / "docker-env"
        captured_env = environment.read_bytes().decode() if environment.exists() else ""
        assert "CARGO_REGISTRIES_" not in result.stdout + result.stderr
        assert "Token Status" not in result.stdout + result.stderr
        for token in _TOKENS:
            if values.get(token):
                assert values[token] not in result.stdout + result.stderr
        return result, args, captured_env

    return run


@pytest.mark.parametrize("reason", [None, "PullRequest", "IndividualCI", "Manual"])
@pytest.mark.parametrize("state", ["absent", "empty", "unresolved"])
def test_missing_tokens_are_optional_under_nounset(run_wrapper, reason, state):
    values = {} if reason is None else {"BUILD_REASON": reason}
    if state != "absent":
        values.update({name: "" if state == "empty" else f"$({name})" for name in _TOKENS})
    result, args, environment = run_wrapper(values)
    assert result.returncode == 0, result.stderr
    assert args == ["run", *_ARGS]
    assert environment == ""


def test_pr_discards_populated_tokens_and_providers(run_wrapper):
    values = {"BUILD_REASON": "PullRequest"}
    values.update({name: uuid.uuid4().hex for name in (*_TOKENS, *_PROVIDERS)})
    result, args, environment = run_wrapper(values)
    assert result.returncode == 0, result.stderr
    assert args == ["run", *_ARGS]
    assert environment == ""


@pytest.mark.parametrize("reason", [None, "IndividualCI", "BatchedCI", "Schedule", "Manual"])
@pytest.mark.parametrize("populated", [(0,), (1,), (0, 1)])
def test_non_pr_forwards_each_available_feed(run_wrapper, reason, populated):
    values = {} if reason is None else {"BUILD_REASON": reason}
    expected = ["run"]
    for index in populated:
        # Generated test data includes shell metacharacters to check argument boundaries.
        values[_TOKENS[index]] = uuid.uuid4().hex + " '\" $() ; &"
        expected.extend(("-e", _TOKENS[index], "-e", f"{_PROVIDERS[index]}=cargo:token"))
    result, args, environment = run_wrapper(values)
    assert result.returncode == 0, result.stderr
    assert args == [*expected, *_ARGS]
    for index, name in enumerate(_TOKENS):
        if index in populated:
            assert f"run\0{name}\0{values[name]}\0" in environment
            assert values[name] not in args
        else:
            assert name not in environment


@pytest.mark.parametrize("missing", [0, 1])
def test_unresolved_feed_does_not_disable_other_feed(run_wrapper, missing):
    populated = 1 - missing
    values = {
        "BUILD_REASON": "IndividualCI",
        _TOKENS[missing]: f"$({_TOKENS[missing]})",
        _PROVIDERS[missing]: "cargo:token",
        _TOKENS[populated]: uuid.uuid4().hex,
    }
    result, args, environment = run_wrapper(values)
    assert result.returncode == 0, result.stderr
    assert args == [
        "run", "-e", _TOKENS[populated], "-e",
        f"{_PROVIDERS[populated]}=cargo:token", *_ARGS,
    ]
    assert _TOKENS[missing] not in environment
    assert _PROVIDERS[missing] not in environment


def test_docker_failure_is_preserved(run_wrapper):
    result, _, _ = run_wrapper({"BUILD_REASON": "PullRequest", "DOCKER_RUN_EXIT": "37"})
    assert result.returncode == 37


def load_template(name):
    return yaml.safe_load((_TEMPLATES / name).read_text(encoding="utf-8"))


def wrapper_steps(value):
    if isinstance(value, dict):
        if "docker-cargo-run.sh" in value.get("script", value.get("bash", "")):
            yield value
        for child in value.values():
            yield from wrapper_steps(child)
    elif isinstance(value, list):
        for child in value:
            yield from wrapper_steps(child)


@pytest.mark.parametrize(
    ("template", "count", "shared"),
    [
        ("build-template-container.yml", 11, {"Build in Container", "Build JavaScript bindings"}),
        ("build-template-alpine.yml", 1, {"Rust Alpine container build"}),
        ("test-mssql-python-odbc-template.yml", 1, {"Build mssql-odbc driver"}),
        ("validation-stages.yml", 1, set()),
    ],
)
def test_pr_capable_callers_do_not_receive_feed_credentials(template, count, shared):
    steps = list(wrapper_steps(load_template(template)))
    assert len(steps) == count
    assert {step["displayName"] for step in steps} >= shared
    for step in steps:
        env = step.get("env", {})
        assert not any(name in env for name in (*_TOKENS, *_PROVIDERS))
        if step["displayName"] in shared:
            assert env[_NON_PR_ENV] == {name: f"$({name})" for name in _TOKENS}
        else:
            assert "eq(variables['Build.Reason'], 'PullRequest')" in step["condition"]
            assert _NON_PR_ENV not in env
        pr_env = {key: value for key, value in env.items() if key != _NON_PR_ENV}
        assert "CARGO_" not in str(pr_env)
        assert "CARGO_" not in step.get("script", step.get("bash", ""))


def test_authentication_and_feed_config_remain_non_pr_only():
    template = load_template("cargo-authenticate-template.yml")

    def steps(value):
        if isinstance(value, dict):
            if "displayName" in value:
                yield value
            for child in value.values():
                yield from steps(child)
        elif isinstance(value, list):
            for child in value:
                yield from steps(child)

    tasks = list(steps(template))
    assert len(tasks) == 3
    assert all(task["condition"] == "ne(variables['Build.Reason'], 'PullRequest')" for task in tasks)


def test_regressions_are_wired_without_removing_existing_selectors():
    stages = load_template("validation-stages.yml")["stages"]
    build = next(stage for stage in stages if stage["stage"] == "Build")
    windows = next(job for job in build["jobs"] if job.get("job") == "Build_Windows")
    step = next(
        step for step in windows["steps"]
        if step.get("displayName") == "Unit test packaging scripts and pipeline configuration"
    )
    command = next(line for line in step["pwsh"].splitlines() if "python -m pytest" in line)
    selectors = {
        "scripts/test_inject_odbc.py",
        "scripts/test_verify_python_wheels.py",
        "scripts/test_release_pipeline.py",
        "scripts/test_validation_pipeline.py",
        "scripts/test_repair_glibc_wheels.py",
        "scripts/test_bump_released_crate_versions.py",
        "scripts/test_python_wheel_install.py",
        "scripts/test_docker_cargo_run.py",
    }
    if (_ROOT / "scripts" / "test_dependency_intake.py").exists():
        selectors.add("scripts/test_dependency_intake.py")
    assert selectors <= set(command.split())
    assert not step.get("continueOnError", False)


def test_linux_job_enforces_bash_regressions():
    stages = load_template("validation-stages.yml")["stages"]
    build = next(stage for stage in stages if stage["stage"] == "Build")
    linux = next(job for job in build["jobs"] if job.get("job") == "Build_Linux")
    step = next(
        step for step in linux["steps"]
        if step.get("displayName") == "Unit test Bash pipeline scripts"
    )
    assert step["bash"].startswith("set -euo pipefail\n")
    command = next(line for line in step["bash"].splitlines() if "-m pytest" in line)
    assert "scripts/test_docker_cargo_run.py" in command.split()
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)

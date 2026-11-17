# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

import pytest
import yaml

ROOT = Path(__file__).parents[1]
SPEC = importlib.util.spec_from_file_location(
    "pool_key_runner", ROOT / ".pipeline" / "scripts" / "run-sql-pool-keys.py"
)
assert SPEC and SPEC.loader
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


@pytest.fixture
def configured_runner(monkeypatch, tmp_path):
    consumer = tmp_path / "consumer" / "mssql_python" / "__init__.py"
    monkeypatch.setitem(sys.modules, "mssql_python", SimpleNamespace(__file__=str(consumer)))
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner.importlib.metadata, "version", lambda _: "1.17.0")
    return tmp_path, consumer


@pytest.mark.parametrize("existing_lock", [False, True])
def test_runner_uses_active_consumer_and_runs_ignored_parser_contract(
    configured_runner, monkeypatch, existing_lock
):
    root, consumer = configured_runner
    manifest = root / "mssql-py-core" / "Cargo.toml"
    lock = manifest.with_name("Cargo.lock")
    if existing_lock:
        lock.parent.mkdir()
        lock.write_text("existing lock", encoding="utf-8")
    calls = []
    monkeypatch.setattr(runner.subprocess, "run", lambda args, **kw: calls.append((args, kw)))
    runner.main()
    assert len(calls) == (2 if existing_lock else 3)
    if not existing_lock:
        assert calls[0][0] == ["cargo", "generate-lockfile", "--manifest-path", str(manifest)]
    assert calls[-2][0] == ["cargo", "fetch", "--locked", "--manifest-path", str(manifest)]
    for _, kwargs in calls:
        assert kwargs["check"] is True
        assert kwargs["cwd"] == root
        assert kwargs["env"]["PYO3_PYTHON"] == sys.executable
    args, kwargs = calls[-1]
    assert args[:3] == ["cargo", "nextest", "run"]
    assert args[args.index("--config-file") + 1] == str(root / ".config" / "nextest.toml")
    assert args[args.index("--run-ignored") + 1] == "all"
    assert args[args.index("--profile") + 1] == "ci"
    assert "test(connection_key::)" in args[args.index("-E") + 1]
    assert "not test(emit_process_key_digest)" in args[args.index("-E") + 1]
    assert "--frozen" in args
    assert kwargs["check"] is True
    assert kwargs["cwd"] == root
    assert kwargs["env"]["PYO3_PYTHON"] == sys.executable
    paths = kwargs["env"]["PYTHONPATH"].split(os.pathsep)
    assert paths[0] == str(consumer.resolve().parents[1])
    assert all(entry in paths for entry in sys.path if entry)
    if existing_lock:
        assert lock.read_text(encoding="utf-8") == "existing lock"


@pytest.mark.parametrize(
    ("existing_lock", "failing_command"),
    [
        (False, "generate-lockfile"),
        (False, "fetch"),
        (False, "nextest"),
        (True, "fetch"),
        (True, "nextest"),
    ],
)
def test_runner_preserves_existing_lock_and_propagates_failure(
    configured_runner, monkeypatch, existing_lock, failing_command
):
    root, _ = configured_runner
    manifest_dir = root / "mssql-py-core"
    lock = manifest_dir / "Cargo.lock"
    if existing_lock:
        manifest_dir.mkdir()
        lock.write_text("existing lock", encoding="utf-8")
    calls = []

    def fail(args, **kwargs):
        calls.append(args)
        if args[1] == failing_command:
            raise subprocess.CalledProcessError(100, args)

    monkeypatch.setattr(runner.subprocess, "run", fail)
    with pytest.raises(subprocess.CalledProcessError) as error:
        runner.main()
    expected = ["fetch", "nextest"] if existing_lock else ["generate-lockfile", "fetch", "nextest"]
    assert [args[1] for args in calls] == expected[: expected.index(failing_command) + 1]
    assert error.value.returncode == 100
    assert error.value.cmd == calls[-1]
    if existing_lock:
        assert lock.read_text(encoding="utf-8") == "existing lock"


@pytest.mark.parametrize("existing_path", [None, "", "existing-python-libs"])
def test_linux_runner_exposes_active_python_shared_library(
    configured_runner, monkeypatch, existing_path
):
    monkeypatch.setattr(runner.sys, "platform", "linux")
    monkeypatch.setattr(
        runner.sysconfig, "get_config_var", lambda name: {"LIBDIR": "python-lib"}[name]
    )
    if existing_path is None:
        monkeypatch.delenv("LD_LIBRARY_PATH", raising=False)
    else:
        monkeypatch.setenv("LD_LIBRARY_PATH", existing_path)
    calls = []
    monkeypatch.setattr(runner.subprocess, "run", lambda args, **kw: calls.append(kw))
    runner.main()
    expected = os.pathsep.join(["python-lib", existing_path]) if existing_path else "python-lib"
    assert all(call["env"]["LD_LIBRARY_PATH"] == expected for call in calls)
    assert os.environ.get("LD_LIBRARY_PATH") == existing_path


@pytest.mark.parametrize("libdir", [None, ""])
def test_linux_runner_rejects_missing_python_library_directory(
    configured_runner, monkeypatch, libdir
):
    monkeypatch.setattr(runner.sys, "platform", "linux")
    monkeypatch.setattr(runner.sysconfig, "get_config_var", lambda _: libdir)
    calls = []
    monkeypatch.setattr(runner.subprocess, "run", lambda args, **kw: calls.append(args))
    with pytest.raises(RuntimeError, match="LIBDIR"):
        runner.main()
    assert calls == []


@pytest.mark.parametrize(
    ("configured_target", "expected"),
    [(None, "10.15"), ("", "10.15"), ("10.15", "10.15"), ("26.0", "26.0"), ("10.12", "10.12")],
)
def test_macos_runner_sets_supported_default_and_preserves_explicit_target(
    configured_runner, monkeypatch, configured_target, expected
):
    monkeypatch.setattr(runner.sys, "platform", "darwin")
    if configured_target is None:
        monkeypatch.delenv("MACOSX_DEPLOYMENT_TARGET", raising=False)
    else:
        monkeypatch.setenv("MACOSX_DEPLOYMENT_TARGET", configured_target)
    calls = []
    monkeypatch.setattr(runner.subprocess, "run", lambda args, **kw: calls.append(kw))
    runner.main()
    assert all(call["env"]["MACOSX_DEPLOYMENT_TARGET"] == expected for call in calls)
    assert os.environ.get("MACOSX_DEPLOYMENT_TARGET") == configured_target


def test_windows_runner_preserves_existing_platform_environment(configured_runner, monkeypatch):
    monkeypatch.setattr(runner.sys, "platform", "win32")
    monkeypatch.setenv("LD_LIBRARY_PATH", "existing-python-libs")
    monkeypatch.setenv("MACOSX_DEPLOYMENT_TARGET", "26.0")
    calls = []
    monkeypatch.setattr(runner.subprocess, "run", lambda args, **kw: calls.append(kw))
    runner.main()
    assert all(call["env"]["LD_LIBRARY_PATH"] == "existing-python-libs" for call in calls)
    assert all(call["env"]["MACOSX_DEPLOYMENT_TARGET"] == "26.0" for call in calls)


def test_key_contract_ci_covers_three_platforms_without_changing_sql_jobs():
    stages = yaml.safe_load(
        (ROOT / ".pipeline" / "templates" / "validation-stages.yml").read_text(encoding="utf-8")
    )["stages"]
    stage = next(item for item in stages if item.get("stage") == "Build_mssql_python")
    job = next(item for item in stage["jobs"] if item.get("job") == "Test_SQL_Pool_Keys")
    assert set(job["strategy"]["matrix"]) == {"Windows", "Linux", "MacOS"}
    assert "condition" not in job
    assert "continueOnError" not in job
    assert {leg["poolKeyImage"] for leg in job["strategy"]["matrix"].values()} == {
        "windows-latest",
        "ubuntu-latest",
        "macOS-latest",
    }
    commands = [step["script"] for step in job["steps"] if "script" in step]
    rust_path = next(step["pwsh"] for step in job["steps"] if "pwsh" in step)
    assert "$env:CARGO_HOME" in rust_path
    assert "##vso[task.prependpath]" in rust_path
    assert "python .pipeline/scripts/run-sql-pool-keys.py" in commands
    assert any("requirements-pool-key.txt" in command for command in commands)
    publication = next(step for step in job["steps"] if step.get("task") == "PublishTestResults@2")
    assert publication["inputs"]["failTaskOnFailedTests"] is True
    assert publication["inputs"]["failTaskOnMissingResultsFile"] is True
    assert any(item.get("job") == "Build_mssql_python_MacOS" for item in stage["jobs"])


def test_runner_suite_is_selected_by_ci():
    stages = yaml.safe_load(
        (ROOT / ".pipeline" / "templates" / "validation-stages.yml").read_text(encoding="utf-8")
    )["stages"]
    stage = next(item for item in stages if item.get("stage") == "Build")
    job = next(item for item in stage["jobs"] if item.get("job") == "Build_Windows")
    step = next(
        item for item in job["steps"] if "python -m pytest scripts/" in item.get("pwsh", "")
    )
    command = next(line for line in step["pwsh"].splitlines() if "python -m pytest" in line)
    assert "scripts/test_sql_pool_keys_runner.py" in command.split()
    assert "-k" not in command.split()
    assert "continueOnError" not in step

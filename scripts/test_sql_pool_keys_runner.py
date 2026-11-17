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


def test_runner_uses_active_consumer_and_runs_ignored_parser_contract(
    configured_runner, monkeypatch
):
    root, consumer = configured_runner
    calls = []
    monkeypatch.setattr(runner.subprocess, "run", lambda args, **kw: calls.append((args, kw)))
    runner.main()
    assert len(calls) == 2
    assert calls[0][0][:2] == ["cargo", "generate-lockfile"]
    args, kwargs = calls[1]
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


def test_runner_preserves_existing_lock_and_propagates_failure(configured_runner, monkeypatch):
    root, _ = configured_runner
    manifest_dir = root / "mssql-py-core"
    manifest_dir.mkdir()
    (manifest_dir / "Cargo.lock").write_text("existing lock", encoding="utf-8")
    calls = []

    def fail(args, **kwargs):
        calls.append(args)
        raise subprocess.CalledProcessError(100, args)

    monkeypatch.setattr(runner.subprocess, "run", fail)
    with pytest.raises(subprocess.CalledProcessError):
        runner.main()
    assert len(calls) == 1
    assert calls[0][:3] == ["cargo", "nextest", "run"]


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

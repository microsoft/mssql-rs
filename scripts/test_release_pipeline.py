# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Local release gates; does not execute publishing, tagging, or ADO jobs."""

from __future__ import annotations

import ast
import itertools
import json
import re
import subprocess
from pathlib import Path

import pytest
import yaml

_ROOT = Path(__file__).parents[1]
_PIPELINE = _ROOT / ".pipeline" / "OneBranch" / "OfficialPythonWheelsRelease.yml"
_METADATA = _ROOT / ".pipeline" / "scripts" / "get-python-release-metadata.ps1"
_SWITCHES = (
    "publishNuGet",
    "publishMssqlTds",
    "publishMssqlMockTds",
    "validateCratesOnly",
    "tagRelease",
)


def evaluate(expression, parameters, succeeded=True):
    """Evaluate only the boolean expression subset used by this pipeline."""
    expression = re.sub(r"\band\(", "all_(", expression)
    expression = re.sub(r"\bor\(", "any_(", expression)

    def visit(node):
        if isinstance(node, ast.Name):
            return {"true": True, "false": False}[node.id.lower()]
        if isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name):
            assert node.value.id == "parameters"
            return parameters[node.attr]
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            args = [visit(arg) for arg in node.args]
            return {
                "eq": lambda a, b: a == b,
                "ne": lambda a, b: a != b,
                "all_": lambda *values: all(values),
                "any_": lambda *values: any(values),
                "succeeded": lambda: succeeded,
            }[node.func.id](*args)
        raise AssertionError(f"Unsupported pipeline expression: {expression}")

    return visit(ast.parse(expression.strip(), mode="eval").body)


def expand(value, parameters):
    if isinstance(value, str):
        return re.sub(
            r"\$\{\{(.+?)\}\}",
            lambda match: str(evaluate(match[1], parameters)).lower(),
            value,
        )
    if isinstance(value, list):
        result = []
        for item in value:
            expanded = expand(item, parameters)
            if isinstance(expanded, list):
                result.extend(expanded)
            elif expanded:
                result.append(expanded)
        return result
    if isinstance(value, dict):
        result = {}
        matched = False
        for key, item in value.items():
            if key.startswith("${{"):
                clause = key[3:-2].strip()
                enabled = not matched if clause == "else" else evaluate(
                    clause.removeprefix("if "), parameters
                )
                matched |= enabled
                if enabled:
                    expanded = expand(item, parameters)
                    if isinstance(expanded, list):
                        assert len(value) == 1
                        return expanded
                    result.update(expanded)
            else:
                result[key] = expand(item, parameters)
        return result
    return value


def test_release_defaults_are_safe():
    pipeline = yaml.safe_load(_PIPELINE.read_text(encoding="utf-8"))
    assert {p["name"]: p["default"] for p in pipeline["parameters"]} == dict.fromkeys(
        _SWITCHES, False
    )
    assert pipeline["trigger"] == "none"
    assert pipeline["pr"] == "none"
    assert pipeline["resources"]["pipelines"][0]["source"] == "Official Python Wheels Build"


@pytest.mark.parametrize("values", list(itertools.product((False, True), repeat=5)))
def test_release_switch_graph(values):
    flags = dict(zip(_SWITCHES, values))
    pipeline = expand(yaml.safe_load(_PIPELINE.read_text(encoding="utf-8")), flags)
    stages = {stage["stage"]: stage for stage in pipeline["extends"]["parameters"]["stages"]}
    nuget, core, mock, dry_run, tag = values
    for stage in stages.values():
        for job in stage["jobs"]:
            assert not any("checkout" in step or "target" in step for step in job["steps"])

    release = stages["Release"]
    assert release["dependsOn"] == []
    assert evaluate(release["condition"], flags) == nuget
    assert not evaluate(release["condition"], flags, succeeded=False)
    assert release["jobs"][0]["variables"]["ob_nugetPublishing_enabled"] == str(nuget).lower()
    # All wheel-only downloads, metadata, validation and packing are in this gated stage.
    for name, stage in stages.items():
        if name != "Release":
            assert "verify-python-wheels" not in str(stage)
            assert "NuGetCommand@" not in str(stage)
            assert not any(
                step.get("download") == "officialBuild" and "artifact" not in step
                for job in stage["jobs"] for step in job["steps"]
            )

    assert ("ReleaseCrates" in stages) == (core or mock or dry_run)
    if "ReleaseCrates" in stages:
        crates = stages["ReleaseCrates"]
        # Explicitly empty: an unrelated NuGet failure/skip cannot block crates.
        assert crates["dependsOn"] == []
        steps = crates["jobs"][0]["steps"]
        tasks = [step["displayName"] for step in steps if step.get("task") == "EsrpRelease@12"]
        assert tasks == (
            (["Publish mssql-tds to crates.io"] if core and not dry_run else [])
            + (["Publish mssql-mock-tds to crates.io"] if mock and not dry_run else [])
        )
        names = [step.get("displayName") for step in steps]
        assert "Validate Rust crate release artifact" in names
        assert ("Preflight mssql-tds version" in names) == core
        assert ("Preflight mssql-mock-tds version" in names) == mock
        assert ("Verify mssql-tds dependency is published" in names) == (mock and not core)
        if core and mock and not dry_run:
            assert names.index("Publish mssql-tds to crates.io") < names.index(
                "Wait for mssql-tds on crates.io"
            ) < names.index("Publish mssql-mock-tds to crates.io")

    assert ("Tag" in stages) == tag
    if tag:
        assert stages["Tag"]["dependsOn"] == ("Release" if nuget else [])
        assert stages["Tag"]["displayName"] == "Tag mssql-py-core Release"
        assert "mssqlTdsCrateVersion" not in str(stages["Tag"])

    for name in ("Release", "Tag"):
        if name not in stages:
            continue
        job = stages[name]["jobs"][0]
        assert job["variables"]["ob_git_fetchDepth"] == 0
        assert job["variables"]["ob_git_persistCredentials"] is True
        metadata_steps = [
            step for step in job["steps"]
            if "get-python-release-metadata.ps1" in step.get("pwsh", "")
        ]
        assert len(metadata_steps) == 1
        step = metadata_steps[0]
        assert step["workingDirectory"] == "$(Build.SourcesDirectory)"
        assert '-RepositoryDirectory "$(Build.SourcesDirectory)"' in step["pwsh"]
        assert '-CommitSha "$(resources.pipeline.officialBuild.sourceCommit)"' in step["pwsh"]
        assert '-SourceBranch "$(resources.pipeline.officialBuild.sourceBranch)"' in step["pwsh"]
        assert ("-IncludeWheelMetadata" in step["pwsh"]) == (name == "Release")
        if name == "Tag":
            git_commands = re.findall(r"^\s*git .+$", step["pwsh"], re.MULTILINE)
            assert len(git_commands) == 4
            assert all('git -C "$(Build.SourcesDirectory)"' in line for line in git_commands)


def git(directory, *arguments):
    return subprocess.run(
        ["git", "-C", str(directory), *arguments],
        check=True, capture_output=True, text=True,
    ).stdout.strip()


def commit_metadata(directory, version, python=True):
    source = directory / "mssql-py-core"
    source.mkdir(exist_ok=True)
    (source / "Cargo.toml").write_text(
        f'[package]\nname = "mssql-py-core"\nversion = "{version}"\n',
        encoding="utf-8",
    )
    if python:
        (source / "pyproject.toml").write_text(
            '[project]\nversion = "0.9.2"\nname = "mssql-python-rs"\n',
            encoding="utf-8",
        )
    git(directory, "add", ".")
    git(directory, "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
        "-c", "commit.gpgsign=false", "commit", "-m", "Synthetic release metadata")
    return git(directory, "rev-parse", "HEAD")


@pytest.fixture
def source_repositories(tmp_path):
    remote = tmp_path / "source"
    remote.mkdir()
    git(remote, "init", "-b", "main")
    commit_metadata(remote, "9.9.9")
    checkout = tmp_path / "checkout"
    git(tmp_path, "clone", str(remote), str(checkout))
    selected = commit_metadata(remote, "0.1.10")
    commit_metadata(remote, "8.8.8")
    return remote, checkout, selected


def read_metadata(checkout, commit, cwd, branch="refs/heads/main", wheels=True):
    # Pass arguments separately; exercise the real helper from a non-repository CWD.
    command = [
        "pwsh", "-NoProfile", "-Command",
        "& { param($script, $repo, $sha, $branch, $wheels) "
        "& $script -RepositoryDirectory $repo -CommitSha $sha -SourceBranch $branch "
        "-IncludeWheelMetadata:($wheels -eq 'true') | ConvertTo-Json -Compress }",
        str(_METADATA), str(checkout), commit, branch, str(wheels).lower(),
    ]
    return subprocess.run(command, cwd=cwd, capture_output=True, text=True, check=False)


def test_metadata_uses_selected_commit_not_checkout_or_branch_tip(source_repositories, tmp_path):
    _, checkout, selected = source_repositories
    checkout_head = git(checkout, "rev-parse", "HEAD")
    result = read_metadata(checkout, selected, tmp_path)
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {
        "SourceCommit": selected, "Version": "0.1.10",
        "DistributionName": "mssql-python-rs", "PythonVersion": "0.9.2",
    }
    assert git(checkout, "rev-parse", "HEAD") == checkout_head


def test_missing_branch_does_not_use_stale_fetch_head(source_repositories, tmp_path):
    _, checkout, selected = source_repositories
    assert read_metadata(checkout, selected, tmp_path).returncode == 0
    result = read_metadata(checkout, selected, tmp_path, branch="refs/heads/missing")
    assert result.returncode != 0
    assert "Could not fetch selected build branch" in result.stderr
    assert '"Version"' not in result.stdout


def test_missing_commit_does_not_use_checkout(source_repositories, tmp_path):
    _, checkout, _ = source_repositories
    result = read_metadata(checkout, "0" * 40, tmp_path)
    assert result.returncode != 0
    assert "was not found after fetching" in result.stderr


def test_missing_git_context_fails_explicitly(source_repositories, tmp_path):
    _, _, selected = source_repositories
    result = read_metadata(tmp_path, selected, tmp_path)
    assert result.returncode != 0
    assert "Could not fetch selected build branch" in result.stderr


def test_tag_metadata_does_not_require_wheel_metadata(source_repositories, tmp_path):
    remote, checkout, _ = source_repositories
    (remote / "mssql-py-core" / "pyproject.toml").unlink()
    selected = commit_metadata(remote, "0.2.0", python=False)
    result = read_metadata(checkout, selected, tmp_path, wheels=False)
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {"SourceCommit": selected, "Version": "0.2.0"}
    result = read_metadata(checkout, selected, tmp_path, wheels=True)
    assert result.returncode != 0
    assert "Could not read mssql-py-core/pyproject.toml" in result.stderr


def test_missing_package_version_does_not_match_dependency(source_repositories, tmp_path):
    remote, checkout, _ = source_repositories
    cargo = remote / "mssql-py-core" / "Cargo.toml"
    cargo.write_text(
        '[package]\nname = "mssql-py-core"\n[dependencies.fake]\nversion = "1.2.3"\n',
        encoding="utf-8",
    )
    git(remote, "add", ".")
    git(remote, "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
        "-c", "commit.gpgsign=false", "commit", "-m", "Missing package version")
    result = read_metadata(checkout, git(remote, "rev-parse", "HEAD"), tmp_path)
    assert result.returncode != 0
    assert "Could not extract [package].version" in result.stderr

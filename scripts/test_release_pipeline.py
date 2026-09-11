# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Local release gates; does not execute publishing, tagging, or ADO jobs."""

from __future__ import annotations

import ast
import hashlib
import io
import itertools
import json
import os
import re
import shutil
import subprocess
import tarfile
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest
import yaml

from test_verify_python_wheels import write_wheel_matrix

_ROOT = Path(__file__).parents[1]
_PIPELINE = _ROOT / ".pipeline" / "OneBranch" / "OfficialPythonWheelsRelease.yml"
_PYPI_PIPELINE = _ROOT / ".pipeline" / "OneBranch" / "PyPIRelease.yml"
_BUILD_STAGES = _ROOT / ".pipeline" / "OneBranch" / "stages.yml"
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
        if value.get("template") == "/.pipeline/templates/validate-release-crates.yml@self":
            template = _ROOT / value["template"].removeprefix("/").removesuffix("@self")
            return expand(yaml.safe_load(template.read_text(encoding="utf-8"))["steps"], parameters)
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


@pytest.mark.parametrize("publish", [False, True])
def test_pypi_release_switch_graph(publish: bool) -> None:
    source = yaml.safe_load(_PYPI_PIPELINE.read_text(encoding="utf-8"))
    assert source["parameters"] == [
        {
            "name": "publishToPyPI",
            "displayName": (
                "Publish selected official wheels to PyPI via ESRP. " "Leave false for a dry run."
            ),
            "type": "boolean",
            "default": False,
        }
    ]
    assert source["trigger"] == "none"
    assert source["pr"] == "none"

    pipeline = expand(source, {"publishToPyPI": publish})
    job = pipeline["extends"]["parameters"]["stages"][0]["jobs"][0]
    steps = job["steps"]
    names = [step.get("displayName") for step in steps]
    assert names.count("Verify and stage official wheels") == 1
    assert ("Require stable branch for publish" in names) == publish
    assert ("ESRP Release mssql-python-rs wheels to PyPI" in names) == publish
    assert ("Release summary" in names) == publish
    assert any(step.get("download") == "officialBuild" for step in steps)
    assert any(step.get("checkout") == "self" for step in steps)
    stage_step = next(
        step for step in steps if step.get("displayName") == "Verify and stage official wheels"
    )
    assert "-RequireOdbc" in stage_step["pwsh"]
    if not publish:
        assert not any(step.get("task", "").startswith("EsrpRelease@") for step in steps)
    else:
        esrp = next(step for step in steps if step.get("task", "").startswith("EsrpRelease@"))
        assert esrp["inputs"]["FolderLocation"] == "$(Agent.TempDirectory)/pypi-publish"


@pytest.mark.parametrize(
    ("release_branch", "build_branch", "succeeds"),
    [
        ("refs/heads/stable", "refs/heads/stable", True),
        ("refs/heads/main", "refs/heads/stable", False),
        ("refs/heads/stable", "refs/heads/main", False),
        ("refs/heads/stable", "refs/heads/STABLE", False),
        ("refs/heads/STABLE", "refs/heads/stable", False),
    ],
)
def test_pypi_publish_requires_both_stable_branches(
    release_branch: str, build_branch: str, succeeds: bool
) -> None:
    pipeline = expand(
        yaml.safe_load(_PYPI_PIPELINE.read_text(encoding="utf-8")),
        {"publishToPyPI": True},
    )
    steps = pipeline["extends"]["parameters"]["stages"][0]["jobs"][0]["steps"]
    script = next(
        step["pwsh"]
        for step in steps
        if step.get("displayName") == "Require stable branch for publish"
    )
    result = subprocess.run(
        ["pwsh", "-NoProfile", "-Command", script],
        capture_output=True,
        text=True,
        check=False,
        env={
            **os.environ,
            "RELEASE_SOURCE_BRANCH": release_branch,
            "OFFICIAL_BUILD_SOURCE_BRANCH": build_branch,
        },
    )

    assert (result.returncode == 0) == succeeds


def prepare_pypi_release(tmp_path: Path, *, duplicate: bool = False):
    remote = tmp_path / "remote"
    remote.mkdir()
    git(remote, "init", "-b", "stable")
    commit_metadata(remote, "0.1.10")
    pyproject = remote / "mssql-py-core" / "pyproject.toml"
    pyproject.write_text(
        '[project]\nname = "mssql-python-rs"\nversion = "0.1.0"\n', encoding="utf-8"
    )
    git(remote, "add", ".")
    git(
        remote,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        "Selected Python release",
    )
    selected = git(remote, "rev-parse", "HEAD")
    pyproject.write_text(
        '[project]\nname = "mssql-python-rs"\nversion = "9.9.9"\n', encoding="utf-8"
    )
    git(remote, "add", ".")
    git(
        remote,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        "Advance branch tip",
    )

    workspace = tmp_path / "workspace"
    checkout = workspace / "s" / "mssql-rs"
    checkout.parent.mkdir(parents=True)
    git(checkout.parent, "clone", str(remote), str(checkout))
    scripts = checkout / ".pipeline" / "scripts"
    scripts.mkdir(parents=True)
    for filename in ("get-python-release-metadata.ps1", "verify-python-wheels.ps1"):
        shutil.copy2(_ROOT / ".pipeline" / "scripts" / filename, scripts)
    wheels = workspace / "officialBuild" / "drop" / "wheels"
    wheels.mkdir(parents=True)
    originals = write_wheel_matrix(wheels)
    if duplicate:
        duplicate_dir = workspace / "officialBuild" / "duplicate" / "wheels"
        duplicate_dir.mkdir(parents=True)
        shutil.copy2(originals[0], duplicate_dir / originals[0].name)

    pipeline = expand(
        yaml.safe_load(_PYPI_PIPELINE.read_text(encoding="utf-8")),
        {"publishToPyPI": False},
    )
    steps = pipeline["extends"]["parameters"]["stages"][0]["jobs"][0]["steps"]
    script = next(
        step["pwsh"]
        for step in steps
        if step.get("displayName") == "Verify and stage official wheels"
    )
    agent_temp = tmp_path / "agent-temp"
    agent_temp.mkdir()
    script = script.replace("$(Pipeline.Workspace)", str(workspace)).replace(
        "$(Agent.TempDirectory)", str(agent_temp)
    )
    result = subprocess.run(
        ["pwsh", "-NoProfile", "-Command", script],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=False,
        env={
            **os.environ,
            "OFFICIAL_BUILD_SOURCE_COMMIT": selected,
            "OFFICIAL_BUILD_SOURCE_BRANCH": "refs/heads/stable",
            "OFFICIAL_BUILD_RUN_ID": "123",
        },
    )
    return result, originals, agent_temp / "pypi-publish", selected


def test_pypi_release_stages_selected_commit_wheels_unchanged(tmp_path: Path) -> None:
    result, originals, staging, selected = prepare_pypi_release(tmp_path)

    assert result.returncode == 0, result.stderr
    assert f"Selected source commit: {selected}" in result.stdout
    assert "Staged 34 mssql-python-rs 0.1.0 wheels unchanged." in result.stdout
    staged = sorted(staging.glob("*.whl"))
    assert [wheel.name for wheel in staged] == sorted(wheel.name for wheel in originals)
    for wheel in originals:
        assert (staging / wheel.name).read_bytes() == wheel.read_bytes()


def test_pypi_release_rejects_duplicate_wheel_names(tmp_path: Path) -> None:
    result, _, staging, _ = prepare_pypi_release(tmp_path, duplicate=True)

    assert result.returncode != 0
    assert "Duplicate wheel filenames in Official Build artifacts" in result.stderr
    assert not list(staging.glob("*.whl"))


def test_crate_templates_resolve_in_self_repository():
    # StageList steps are expanded inside GovernedTemplates, so relative paths
    # without @self can resolve against the wrong repository.
    templates = re.findall(
        r"^\s*-\s*template:\s*(.*validate-release-crates.*)$",
        _PIPELINE.read_text(encoding="utf-8"),
        re.MULTILINE,
    )
    assert templates == ["/.pipeline/templates/validate-release-crates.yml@self"] * 6


@pytest.mark.parametrize("architecture", ("x64", "ARM64"))
@pytest.mark.parametrize("build_odbc", (False, True))
def test_manylinux_repair_does_not_depend_on_odbc(architecture: str, build_odbc: bool) -> None:
    flags = {
        "buildAllTargets": True,
        "buildPythonWheels": True,
        "buildOdbcNative": build_odbc,
        "buildRustCrates": False,
        "isOfficial": False,
        "publishToFeed": True,
    }
    pipeline = expand(yaml.safe_load(_BUILD_STAGES.read_text(encoding="utf-8")), flags)
    build = next(stage for stage in pipeline["stages"] if stage["stage"] == "Build")
    job = next(job for job in build["jobs"] if job["job"] == f"Linux_{architecture}")
    names = [step.get("displayName") for step in job["steps"]]
    repair = f"Repair glibc wheels into manylinux (Linux {architecture})"
    injection = f"Inject ODBC driver into wheels (Linux {architecture})"

    assert names.count(repair) == 1
    assert (injection in names) == build_odbc
    if build_odbc:
        assert names.index(injection) < names.index(repair)


@pytest.mark.parametrize(
    ("build_reason", "is_official", "expected_version"),
    [
        ("Schedule", False, "0.1.0-nightly.20260910"),
        ("IndividualCI", False, "0.1.0-dev.20260910.12345"),
        ("Manual", False, "0.1.0-dev.20260910.12345"),
        ("Manual", True, "0.1.0"),
    ],
)
def test_nonofficial_nuget_versions_follow_python_distribution(
    tmp_path: Path, build_reason: str, is_official: bool, expected_version: str
) -> None:
    source = tmp_path / "source"
    package = source / "mssql-py-core"
    package.mkdir(parents=True)
    (package / "Cargo.toml").write_text(
        '[package]\nname = "mssql-py-core"\nversion = "0.1.10"\n', encoding="utf-8"
    )
    (package / "pyproject.toml").write_text(
        '[project]\nname = "mssql-python-rs"\nversion = "0.1.0"\n', encoding="utf-8"
    )
    staging = tmp_path / "staging"
    staging.mkdir()
    flags = {
        "buildAllTargets": True,
        "buildPythonWheels": True,
        "buildOdbcNative": True,
        "buildRustCrates": False,
        "isOfficial": is_official,
        "publishToFeed": True,
    }
    pipeline = expand(yaml.safe_load(_BUILD_STAGES.read_text(encoding="utf-8")), flags)
    publish = next(stage for stage in pipeline["stages"] if stage["stage"] == "Publish")
    step = next(
        step
        for step in publish["jobs"][0]["steps"]
        if step.get("displayName") == "Generate NuGet package metadata"
    )
    script = step["pwsh"].replace(
        '$dateStamp = Get-Date -Format "yyyyMMdd"', '$dateStamp = "20260910"'
    )
    variables = {
        "Build.SourcesDirectory": str(source),
        "Build.StagingDirectory": str(staging),
        "Build.Reason": build_reason,
        "Build.BuildId": "12345",
        "Build.SourceVersion": "a" * 40,
        "Build.BuildNumber": "20260910.1",
    }
    for key, value in variables.items():
        script = script.replace(f"$({key})", value)

    result = subprocess.run(
        ["pwsh", "-NoProfile", "-Command", script],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode == 0, result.stderr
    assert f"Package version: {expected_version}" in result.stdout
    metadata = ET.parse(staging / "mssql-python-rs-wheels.nuspec").find("metadata")
    assert metadata.findtext("version") == expected_version


@pytest.mark.parametrize("values", list(itertools.product((False, True), repeat=5)))
def test_release_switch_graph(values):
    flags = dict(zip(_SWITCHES, values))
    pipeline = expand(yaml.safe_load(_PIPELINE.read_text(encoding="utf-8")), flags)
    stages = {stage["stage"]: stage for stage in pipeline["extends"]["parameters"]["stages"]}
    nuget, core, mock, dry_run, tag = values
    for stage in stages.values():
        for job in stage["jobs"]:
            custom = job["pool"].get("isCustom", False)
            assert not any("target" in step for step in job["steps"])
            assert sum("checkout" in step for step in job["steps"]) == int(custom)
            if custom:
                assert stage["stage"] == "ReleaseCrates"
                assert job["pool"] == {
                    "type": "windows", "isCustom": True,
                    "name": "Azure Pipelines", "vmImage": "windows-2022",
                }
                assert "ESRP Federated" not in str(job)
                assert not any(step.get("task") in ("EsrpRelease@12", "NuGetCommand@2")
                               for step in job["steps"])
            else:
                assert "test-cratesio-package.ps1" not in str(job)
            assert "continueOnError" not in job
            assert all("continueOnError" not in step for step in job["steps"])

    release = stages["Release"]
    assert release["dependsOn"] == []
    assert evaluate(release.get("condition", "succeeded()"), flags)
    assert not evaluate(release.get("condition", "succeeded()"), flags, succeeded=False)
    assert len(release["jobs"]) == 1
    assert release["jobs"][0]["job"] == "PublishRelease"
    assert release["jobs"][0]["variables"]["ob_nugetPublishing_enabled"] == str(nuget).lower()
    release_steps = release["jobs"][0]["steps"]
    release_names = [step.get("displayName") for step in release_steps]
    assert any(step.get("download") == "officialBuild" for step in release_steps)
    assert "Validate official Python wheels" in release_names
    for name in ("Prepare release NuGet package", "Create NuGet package", "Verify NuGet package"):
        assert (name in release_names) == nuget
        if nuget:
            assert release_names.index("Validate official Python wheels") < release_names.index(name)
    assert any(step.get("task") == "NuGetCommand@2" for step in release_steps) == nuget
    if not nuget:
        assert ".nuspec" not in str(release_steps)
        assert ".nupkg" not in str(release_steps)
    assert pipeline["extends"]["parameters"]["nugetPublishing"]["feeds"] == [
        {"name": "public/mssql-rs_Public", "continueOnConflict": True}
    ]
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
        # Explicitly empty: wheel validation or NuGet failure cannot block crates.
        assert crates["dependsOn"] == []
        jobs = {job["job"]: job for job in crates["jobs"]}
        expected_jobs = {"ValidateCrates"}
        if core or mock:
            expected_jobs.add("RegistryPreflight")
        if not dry_run:
            if core:
                expected_jobs.update(("PublishCore", "CoreAvailable"))
            if mock:
                expected_jobs.update(("PublishMock", "MockAvailable"))
        assert set(jobs) == expected_jobs
        steps = [step for job in jobs.values() for step in job["steps"]]
        tasks = [step["displayName"] for step in steps if step.get("task") == "EsrpRelease@12"]
        assert tasks == (
            (["Publish mssql-tds to crates.io"] if core and not dry_run else [])
            + (["Publish mssql-mock-tds to crates.io"] if mock and not dry_run else [])
        )
        names = [step.get("displayName") for step in steps]
        for job in jobs.values():
            downloads = [step for step in job["steps"] if "download" in step]
            assert len(downloads) == 1
            assert downloads[0]["download"] == "officialBuild"
            assert downloads[0]["artifact"] == "drop_Build_RustCrates"
            job_names = [step.get("displayName") for step in job["steps"]]
            validation_index = job_names.index("Validate Rust crate release artifact")
            assert validation_index == (2 if job["pool"].get("isCustom") else 1)
            validation = job["steps"][validation_index]
            assert "$(Pipeline.Workspace)/officialBuild/drop_Build_RustCrates" in validation["pwsh"]
        if core or mock:
            assert jobs["RegistryPreflight"]["dependsOn"] == "ValidateCrates"
        assert ("Preflight mssql-tds version" in names) == core
        assert ("Preflight mssql-mock-tds version" in names) == mock
        assert ("Verify mssql-tds dependency is published" in names) == (mock and not core)
        assert ("Wait for mssql-tds on crates.io" in names) == (core and not dry_run)
        assert ("Verify mssql-mock-tds on crates.io" in names) == (mock and not dry_run)
        if core and not dry_run:
            assert jobs["PublishCore"]["dependsOn"] == "RegistryPreflight"
            assert jobs["CoreAvailable"]["dependsOn"] == "PublishCore"
        if mock and not dry_run:
            assert jobs["PublishMock"]["dependsOn"] == ("CoreAvailable" if core else "RegistryPreflight")
            assert jobs["MockAvailable"]["dependsOn"] == "PublishMock"
        for job in jobs.values():
            for step in job["steps"]:
                if step.get("task") == "EsrpRelease@12":
                    assert job["templateContext"] == {"type": "releaseJob", "isProduction": True}
                    assert job["variables"][0] == {"group": "ESRP Federated Creds (AME)"}
                    assert step["inputs"]["waitforreleasecompletion"] is True
                    assert step["inputs"]["usemanagedidentity"] is True
                    assert step["inputs"]["contenttype"] == "Rust"
                    assert step["inputs"]["intent"] == "PackageDistribution"
                    assert step["inputs"]["connectedservicename"] == (
                        "ESRP Managed Identity federated auth - AME tenant-mssql-rs"
                    )
                    assert step["inputs"]["mainpublisher"] == "ESRPRELPACMAN"
                    assert step["inputs"]["domaintenantid"] == "975f013f-7f24-47e8-a7d3-abc4752bf346"

    assert ("Tag" in stages) == tag
    if tag:
        assert stages["Tag"]["dependsOn"] == "Release"
        assert not evaluate(stages["Tag"].get("condition", "succeeded()"), flags, succeeded=False)
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
        if name == "Release":
            assert step["displayName"] == "Validate official Python wheels"
            assert "condition" not in step
            assert "verify-python-wheels.ps1" in step["pwsh"]
            assert ".nuspec" not in step["pwsh"]
            for variable in ("releaseVersion", "sourceCommit", "releaseDistributionName"):
                assert step["pwsh"].index("verify-python-wheels.ps1") < step["pwsh"].index(
                    f"task.setvariable variable={variable}"
                )
                if nuget:
                    preparation = release_steps[release_names.index("Prepare release NuGet package")]
                    assert f"$({variable})" in preparation["pwsh"]
        if name == "Tag":
            git_commands = re.findall(r"^\s*git .+$", step["pwsh"], re.MULTILINE)
            assert len(git_commands) == 4
            assert all('git -C "$(Build.SourcesDirectory)"' in line for line in git_commands)


@pytest.mark.parametrize(("core", "failed_gate"), [
    (core, gate)
    for core in (False, True)
    for gate in (
        "PublishRelease", "ValidateCrates", "RegistryPreflight",
        "PublishCore", "CoreAvailable", "PublishMock", "MockAvailable",
    )
    if core or gate not in ("PublishCore", "CoreAvailable")
])
@pytest.mark.parametrize("result", ["Failed", "Canceled", "Skipped"])
def test_release_gate_failure_propagation(core, failed_gate, result):
    flags = dict.fromkeys(_SWITCHES, True) | {
        "publishMssqlTds": core, "validateCratesOnly": False,
    }
    pipeline = expand(yaml.safe_load(_PIPELINE.read_text(encoding="utf-8")), flags)
    stages = {stage["stage"]: stage for stage in pipeline["extends"]["parameters"]["stages"]}
    results = {}
    for name in ("Release", "ReleaseCrates"):
        assert stages[name]["dependsOn"] == []
        for job in stages[name]["jobs"]:
            # These jobs use the native succeeded() default, not always() or a
            # flag-only condition that could run after a skipped/failed gate.
            assert "condition" not in job
            dependency = job.get("dependsOn")
            if job["job"] == failed_gate:
                results[job["job"]] = result
            elif dependency and results[dependency] != "Succeeded":
                results[job["job"]] = "Skipped"
            else:
                results[job["job"]] = "Succeeded"
    if failed_gate == "PublishRelease":
        assert results["MockAvailable"] == "Succeeded"
    else:
        assert results["PublishRelease"] == "Succeeded"
        assert results["MockAvailable"] != "Succeeded"
        if failed_gate not in ("PublishMock", "MockAvailable"):
            assert results["PublishMock"] == "Skipped"
        if core and failed_gate in ("ValidateCrates", "RegistryPreflight"):
            assert results["PublishCore"] == "Skipped"
    assert stages["Tag"]["dependsOn"] == "Release"


def run_cratesio_script(tmp_path, responses, arguments):
    # Shadow HTTP and sleep only; execute the real PowerShell gate, never a
    # Python reimplementation of its response handling.
    data = json.dumps(responses).replace("'", "''")
    script = str(_ROOT / ".pipeline" / "scripts" / "test-cratesio-package.ps1").replace("'", "''")
    command = f"""
    $ErrorActionPreference = 'Stop'
    $global:Responses = @('{data}' | ConvertFrom-Json)
    $global:RequestCount = 0
    function Invoke-WebRequest {{
        param($Uri, $Headers, [switch]$SkipHttpErrorCheck, $TimeoutSec)
        Write-Host "REQUEST:$Uri"
        if ($global:RequestCount -ge $global:Responses.Count) {{ throw 'Unexpected HTTP request' }}
        $response = $global:Responses[$global:RequestCount++]
        if ($response.error) {{ throw $response.error }}
        return $response
    }}
    function Start-Sleep {{ param($Seconds) Write-Host "SLEEP:$Seconds" }}
    & '{script}' {arguments}
    """
    return subprocess.run(
        ["pwsh", "-NoProfile", "-Command", command], cwd=tmp_path,
        capture_output=True, text=True, check=False,
    )


@pytest.mark.parametrize(("state", "responses", "success", "requests"), [
    ("Absent", [404], True, 1),
    ("Absent", [200], False, 1),
    ("Absent", [401], False, 1),
    ("Absent", [403], False, 1),
    ("Absent", [429], False, 1),
    ("Absent", [503], False, 1),
    ("Absent", ["socket access forbidden"], False, 1),
    ("Available", [200], True, 1),
    ("Available", [404, 200], True, 2),
    ("Available", [429, 503, 200], True, 3),
    ("Available", ["temporary transport failure", 200], True, 2),
    ("Available", [404, 404, 404], False, 3),
    ("Available", [503, 503, 503], False, 3),
    ("Available", ["socket access forbidden"] * 3, False, 3),
    ("Available", [301], False, 1),
    ("Available", [401], False, 1),
    ("Available", [403], False, 1),
])
def test_cratesio_http_states(tmp_path, state, responses, success, requests):
    responses = [
        {"StatusCode": item} if isinstance(item, int) else {"error": item}
        for item in responses
    ]
    result = run_cratesio_script(
        tmp_path, responses,
        f"-CrateName mssql-tds -Version 0.1.0 -ExpectedState {state} -MaxAttempts 3 -DelaySeconds 1",
    )
    assert (result.returncode == 0) == success, result.stderr
    assert result.stdout.count("REQUEST:") == requests
    if not success:
        assert "is not published" not in result.stdout
        assert "is available" not in result.stdout


@pytest.mark.parametrize(("damage", "damaged_name"), [
    (None, None),
    ("hash", "mssql-tds"),
    ("hash", "mssql-mock-tds"),
    ("dependency", "mssql-mock-tds"),
    ("missing", "mssql-tds"),
    ("missing", "mssql-mock-tds"),
    ("extra", None),
    ("order", None),
])
def test_crate_artifact_revalidation(tmp_path, damage, damaged_name):
    entries = []
    for name in ("mssql-tds", "mssql-mock-tds"):
        folder = tmp_path / name
        folder.mkdir()
        crate = folder / f"{name}-0.1.0.crate"
        dependency = "9.9.9" if damage == "dependency" else "0.1.0"
        manifest = f'[package]\nname = "{name}"\nversion = "0.1.0"\n'
        if name == "mssql-mock-tds":
            manifest += f'\n[dependencies.mssql-tds]\nversion = "{dependency}"\n'
        with tarfile.open(crate, "w:gz") as archive:
            content = manifest.encode("utf-8")
            member = tarfile.TarInfo(f"{name}-0.1.0/Cargo.toml")
            member.size = len(content)
            archive.addfile(member, io.BytesIO(content))
        entries.append({
            "name": name, "version": "0.1.0", "file": f"{name}/{crate.name}",
            "sha256": hashlib.sha256(crate.read_bytes()).hexdigest(),
        })
    if damage in ("hash", "missing"):
        damaged_crate = tmp_path / damaged_name / f"{damaged_name}-0.1.0.crate"
        if damage == "hash":
            damaged_crate.write_bytes(b"replaced after preflight")
        else:
            damaged_crate.unlink()
    elif damage == "extra":
        (tmp_path / "extra.crate").write_bytes(b"unexpected")
    elif damage == "order":
        entries.reverse()
    (tmp_path / "release-manifest.json").write_text(
        json.dumps({"schemaVersion": 1, "crates": entries}), encoding="utf-8",
    )
    result = subprocess.run(
        ["pwsh", "-NoProfile", "-File",
         str(_ROOT / ".pipeline" / "scripts" / "validate-crate-release-artifact.ps1"),
         "-ArtifactDirectory", str(tmp_path)],
        capture_output=True, text=True, check=False,
    )
    assert (result.returncode == 0) == (damage is None), result.stderr
    assert ("Rust crate release artifact is valid." in result.stdout) == (damage is None)


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


@pytest.mark.parametrize("nuget,wheels_present", [(False, False), (False, True), (True, True)])
def test_wheel_validation_and_optional_nuspec(source_repositories, tmp_path, nuget, wheels_present):
    remote, checkout, _ = source_repositories
    (remote / "mssql-py-core" / "pyproject.toml").write_text(
        '[project]\nname = "mssql-python-rs"\nversion = "0.1.0"\n', encoding="utf-8"
    )
    selected = commit_metadata(remote, "0.1.10", python=False)
    scripts = checkout / ".pipeline" / "scripts"
    scripts.mkdir(parents=True)
    for filename in ("get-python-release-metadata.ps1", "verify-python-wheels.ps1"):
        shutil.copy2(_ROOT / ".pipeline" / "scripts" / filename, scripts / filename)

    artifact_wheels = tmp_path / "officialBuild" / "drop" / "wheels"
    artifact_wheels.mkdir(parents=True)
    if wheels_present:
        write_wheel_matrix(artifact_wheels)
    staging = tmp_path / "staging"
    flags = dict.fromkeys(_SWITCHES, False) | {"publishNuGet": nuget}
    pipeline = expand(yaml.safe_load(_PIPELINE.read_text(encoding="utf-8")), flags)
    steps = pipeline["extends"]["parameters"]["stages"][0]["jobs"][0]["steps"]
    variables = {
        "Build.SourcesDirectory": str(checkout),
        "Build.StagingDirectory": str(staging),
        "Pipeline.Workspace": str(tmp_path),
        "resources.pipeline.officialBuild.sourceCommit": selected,
        "resources.pipeline.officialBuild.sourceBranch": "refs/heads/main",
        "resources.pipeline.officialBuild.runID": "123",
    }

    def run_step(name):
        script = next(step["pwsh"] for step in steps if step.get("displayName") == name)
        for key, value in variables.items():
            script = script.replace(f"$({key})", value)
        return subprocess.run(
            ["pwsh", "-NoProfile", "-Command", script],
            cwd=tmp_path, capture_output=True, text=True, check=False,
        )

    result = run_step("Validate official Python wheels")
    assert not list(staging.glob("*.nuspec"))
    assert not list(staging.glob("**/*.nupkg"))
    if not wheels_present:
        assert result.returncode != 0
        assert "No wheels found to validate" in result.stderr
        assert "task.setvariable" not in result.stdout
        return

    assert result.returncode == 0, result.stderr
    assert "Validated 34 mssql-python-rs wheels" in result.stdout
    variables.update(re.findall(r"##vso\[task.setvariable variable=(\w+)\](.*)", result.stdout))
    assert variables["releaseVersion"] == "0.1.0"
    assert variables["sourceCommit"] == selected
    assert variables["releaseDistributionName"] == "mssql-python-rs"
    if nuget:
        result = run_step("Prepare release NuGet package")
        assert result.returncode == 0, result.stderr
        metadata = ET.parse(staging / "mssql-python-rs-wheels.nuspec").find("metadata")
        assert metadata.findtext("version") == "0.1.0"
        assert selected[:8] in metadata.findtext("description")
        assert "mssql-python-rs" in metadata.findtext("description")


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


def test_metadata_rejects_commit_outside_selected_branch(source_repositories, tmp_path):
    _, checkout, _ = source_repositories
    git(checkout, "checkout", "--orphan", "unrelated")
    git(checkout, "rm", "-rf", ".")
    (checkout / "unrelated.txt").write_text("unrelated\n", encoding="utf-8")
    git(checkout, "add", ".")
    git(
        checkout,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        "Unrelated commit",
    )
    unrelated = git(checkout, "rev-parse", "HEAD")

    result = read_metadata(checkout, unrelated, tmp_path)

    assert result.returncode != 0
    assert "reachable from refs/heads/main" in result.stderr


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

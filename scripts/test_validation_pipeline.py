# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Regression tests for validation jobs and artifact publication."""

from pathlib import Path

import pytest
import yaml

_ROOT = Path(__file__).parents[1]
_TEMPLATES = _ROOT / ".pipeline" / "templates"
_NON_PR = "and(succeeded(), ne(variables['Build.Reason'], 'PullRequest'))"
_PR = "and(succeeded(), eq(variables['Build.Reason'], 'PullRequest'))"


def load_template(name):
    return yaml.safe_load((_TEMPLATES / name).read_text(encoding="utf-8"))


@pytest.mark.parametrize(
    ("job_name", "architecture"), [("Build_Linux", "x64"), ("Build_Linux_ARM", "ARM64")]
)
def test_linux_jobs_keep_glibc_and_alpine_builds(job_name, architecture):
    stages = load_template("validation-stages.yml")["stages"]
    build = next(stage for stage in stages if stage["stage"] == "Build")
    job = next(job for job in build["jobs"] if job.get("job") == job_name)
    templates = {step["template"]: step for step in job["steps"] if "template" in step}
    container = templates["build-template-container.yml"]
    alpine = templates["build-template-alpine.yml"]
    assert container["parameters"]["architecture"] == architecture
    assert "condition" not in container
    assert "condition" not in alpine
    assert job["steps"].index(container) < job["steps"].index(alpine)


@pytest.mark.parametrize(
    ("template", "archive"),
    [
        ("build-template-container.yml", "tdslib-nextest.tar.zst"),
        ("build-template-alpine.yml", "tdslib-nextest-musl.tar.zst"),
    ],
)
def test_only_nextest_archives_are_staged_on_non_pr_runs(template, archive):
    steps = load_template(template)["steps"]
    staging = [
        step for step in steps if "$(Build.ArtifactStagingDirectory)" in str(step)
    ]
    copies = [step for step in staging if step.get("task") != "PublishBuildArtifacts@1"]
    assert len(copies) == 1
    copy = copies[0]
    assert copy["condition"] == _NON_PR
    assert "cp " in copy["script"]
    assert archive in copy["script"]
    assert "$(Build.ArtifactStagingDirectory)" in copy["script"]
    assert "docker-cargo-run.sh" not in copy["script"]


def test_linux_drop_publication_is_non_pr_only():
    steps = load_template("build-template-alpine.yml")["steps"]
    publishes = [step for step in steps if step.get("task") == "PublishBuildArtifacts@1"]
    assert len(publishes) == 1
    assert publishes[0]["condition"] == _NON_PR
    assert publishes[0]["inputs"] == {
        "PathtoPublish": "$(Build.ArtifactStagingDirectory)",
        "ArtifactName": "$(System.PhaseName)",
        "publishLocation": "Container",
    }


def test_alpine_gssapi_compilation_still_runs_on_prs():
    steps = load_template("build-template-alpine.yml")["steps"]
    build = next(
        step for step in steps
        if step.get("displayName") == "Rust Alpine container build"
    )
    assert build["condition"] == "succeeded()"
    assert "/workspace/scripts/dockerentry/alpine-build.sh" in build["script"]
    script = (_ROOT / "scripts" / "dockerentry" / "alpine-build.sh").read_text(
        encoding="utf-8"
    )
    assert "cargo nextest archive" in script
    assert "--features gssapi" in script


@pytest.mark.parametrize("architecture", ["x64", "ARM64"])
def test_linux_compilation_and_pr_tests_remain_enabled(architecture):
    steps = load_template("build-template-container.yml")["steps"]
    build = next(step for step in steps if step.get("displayName") == "Build in Container")
    assert build.get("condition", "succeeded()") == "succeeded()"
    operator = "eq" if architecture == "ARM64" else "ne"
    branch = f"${{{{ if {operator}(parameters.architecture, 'ARM64') }}}}"
    tests = next(
        step for group in steps for step in group.get(branch, [])
        if step.get("displayName") == "Run Tests and Coverage"
    )
    assert tests["condition"] == _PR
    assert "/workspace/.pipeline/scripts/containerized-test.sh" in tests["script"]


def test_miri_is_limited_to_windows_and_linux_x64_pr_jobs():
    build = next(
        stage for stage in load_template("validation-stages.yml")["stages"]
        if stage["stage"] == "Build"
    )
    expected = {
        "Build_Windows": ("pwsh", "x86_64-pc-windows-msvc", "Windows x64"),
        "Build_Linux": ("bash", "x86_64-unknown-linux-gnu", "Linux x64"),
    }
    found = set()
    for job in build["jobs"]:
        runs = [
            step for step in job.get("steps", [])
            if step.get("displayName", "").startswith("Run ODBC Miri tests")
        ]
        if not runs:
            continue
        found.add(job["job"])
        shell, target, label = expected[job["job"]]
        assert len(runs) == 1
        run = runs[0]
        assert run["condition"] == _PR
        assert not run.get("continueOnError", False)
        command = run[shell]
        assert "rustup toolchain install nightly-2026-09-06" in command
        assert "--component miri,rust-src" in command
        assert f"miri setup --target {target}" in command
        assert "cargo +nightly-2026-09-06 miri nextest run" in command
        assert f"--target {target}" in command
        for argument in (
            "--frozen", "--package mssqlodbc", "--lib",
            "--profile miri-odbc", "--no-fail-fast", "--no-tests=fail",
        ):
            assert argument in command
        if shell == "pwsh":
            assert command.count("if ($LASTEXITCODE -ne 0) { throw ") == 3
            assert run["env"]["MIRIFLAGS"] == "-Zmiri-seed=0"
        else:
            assert command.count("set -euo pipefail") == 2
            assert "docker-cargo-run.sh --rm" in command
            assert "ghcr.io/microsoft/mssql-rs/build/ubuntu:22.04" in command
            assert "-e MIRIFLAGS=-Zmiri-seed=0" in command
        publish = next(
            step for step in job["steps"]
            if step.get("displayName") == f"Publish ODBC Miri test results ({label})"
        )
        assert job["steps"].index(run) < job["steps"].index(publish)
        assert publish["task"] == "PublishTestResults@2"
        assert publish["condition"] == _PR.replace("succeeded()", "succeededOrFailed()")
        assert publish["inputs"]["testResultsFormat"] == "JUnit"
        assert publish["inputs"]["testResultsFiles"] == (
            "$(Build.SourcesDirectory)/target/nextest/miri-odbc/junit.xml"
        )
        assert publish["inputs"]["failTaskOnFailedTests"] is True
        assert publish["inputs"]["failTaskOnMissingResultsFile"] is True
    assert found == expected.keys()

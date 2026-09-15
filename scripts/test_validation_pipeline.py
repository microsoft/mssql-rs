# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Regression tests for validation pipeline builds, tests, and artifacts."""

import re
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


def test_mssql_python_odbc_failures_fail_the_job():
    template_path = _TEMPLATES / "test-mssql-python-odbc-template.yml"
    template = template_path.read_text(encoding="utf-8")
    steps = yaml.safe_load(template)["steps"]
    test_step = next(
        step
        for step in steps
        if step.get("displayName") == "Run mssql-python tests against mssql-odbc"
    )
    assert "continueOnError" not in test_step
    assert 'exit "$rc"' in test_step["script"]
    assert "task.complete result=SucceededWithIssues" not in test_step["script"]
    assert "SucceededWithIssues" not in template

    # A docker-exec launch failure (125/126/127), or the runner's own exit 2
    # for a broken harness, both mean the tests said nothing about the driver,
    # so each must still fail the job (exit "$rc") through a branch that says
    # so rather than reporting it as a driver test failure.
    assert re.search(r"\b2\)", test_step["script"])
    assert re.search(r"125\|126\|127\)", test_step["script"])
    assert "exit \"$rc\"" in test_step["script"].rsplit("esac", 1)[-1]

    # The step alone isn't the whole gate: a job-level continueOnError would
    # silently restore the old non-blocking behavior regardless of exit code.
    stages = yaml.safe_load(
        (_TEMPLATES / "validation-stages.yml").read_text(encoding="utf-8")
    )["stages"]
    build_mssql_python = next(stage for stage in stages if stage["stage"] == "Build_mssql_python")
    odbc_job = next(
        job
        for job in build_mssql_python["jobs"]
        if job.get("job") == "Test_mssql_python_on_mssql_odbc"
    )
    assert "continueOnError" not in odbc_job

    # A dirty run must still exit nonzero from the runner itself, not just from
    # the step that wraps it.
    runner = (_ROOT / ".pipeline" / "scripts" / "run-mssql-python-odbc-tests.sh").read_text(
        encoding="utf-8"
    )
    dirty_run_parts = runner.split('if [ "$failed" -gt 0 ]', 1)
    assert len(dirty_run_parts) == 2, "dirty-run guard line not found in runner script"
    assert re.search(r"\bexit 1\b", dirty_run_parts[1])


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


def test_macos_pr_runs_native_odbc_e2e_against_existing_sql():
    stages = load_template("validation-stages.yml")["stages"]
    build = next(stage for stage in stages if stage["stage"] == "Build")
    job = next(job for job in build["jobs"] if job.get("job") == "Test_MacOS")
    assert job["pool"]["vmImage"] == "macOS-latest"
    steps = job["steps"]
    sql = next(step for step in steps if step.get("template") == "sql-setup-template.yml")
    assert sql["parameters"]["buildTarget"] == "MacOS"
    verify = next(
        step for step in steps
        if step.get("displayName") == "Verify macOS SQL Server before tests"
    )
    rust = next(step for step in steps if step.get("template") == "build-template.yml")
    assert rust["parameters"].get("enableTests", True) is True
    install = next(
        step for step in steps
        if step.get("displayName") == "Install ODBC C++ e2e dependencies (macOS)"
    )
    run = next(
        step for step in steps
        if step.get("displayName") == "Run ODBC C++ e2e tests (macOS)"
    )
    publish = next(
        step for step in steps
        if step.get("displayName") == "Publish ODBC e2e test results (macOS)"
    )
    order = [steps.index(step) for step in (sql, verify, rust, install, run, publish)]
    assert order == sorted(order)
    for step in (install, run):
        assert step["condition"] == _PR
        assert "set -euo pipefail" in step["script"]
        assert not step.get("continueOnError", False)
    assert "command -v cmake" in install["script"]
    assert "brew install cmake" in install["script"]
    assert "brew install unixodbc" in install["script"]
    assert 'CMAKE_PREFIX_PATH="$(brew --prefix unixodbc)' in run["script"]
    assert "export CMAKE_PREFIX_PATH\n" in run["script"]
    assert "bash mssql-odbc/tests/e2e/run_e2e.sh --retries=3" in run["script"]
    assert "docker" not in run["script"]
    assert run["env"] == {
        "ODBC_TEST_SERVER": "127.0.0.1,1433",
        "ODBC_TEST_UID": "sa",
        "ODBC_TEST_PWD": "$(SQL_PASSWORD)",
        "ODBC_TEST_TRUST_CERT": "Yes",
        "RUST_BACKTRACE": "full",
    }
    assert publish["task"] == "PublishTestResults@2"
    assert publish["condition"] == (
        "and(succeededOrFailed(), eq(variables['Build.Reason'], 'PullRequest'))"
    )
    assert not publish.get("continueOnError", False)
    assert publish["inputs"] == {
        "testResultsFormat": "JUnit",
        "testResultsFiles": (
            "$(Build.SourcesDirectory)/mssql-odbc/tests/e2e/build/junit-mssql-odbc.xml"
        ),
        "testRunTitle": "$(System.PhaseName)-OdbcE2E",
        "failTaskOnFailedTests": True,
        "failTaskOnMissingResultsFile": True,
    }

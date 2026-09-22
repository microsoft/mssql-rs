# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Regression tests for validation pipeline builds, tests, and artifacts."""

import re
import shutil
import subprocess
from pathlib import Path

import pytest
import yaml

_ROOT = Path(__file__).parents[1]
_TEMPLATES = _ROOT / ".pipeline" / "templates"
_NON_PR = "and(succeeded(), ne(variables['Build.Reason'], 'PullRequest'))"
_PR = "and(succeeded(), eq(variables['Build.Reason'], 'PullRequest'))"


def load_template(name):
    return yaml.safe_load((_TEMPLATES / name).read_text(encoding="utf-8"))


def test_version_bump_tests_run_in_shared_validation():
    stages = load_template("validation-stages.yml")["stages"]
    build = next(stage for stage in stages if stage["stage"] == "Build")
    windows = next(job for job in build["jobs"] if job.get("job") == "Build_Windows")
    step = next(
        step for step in windows["steps"]
        if step.get("displayName") == "Unit test packaging scripts and pipeline configuration"
    )
    command = next(line for line in step["pwsh"].splitlines() if "python -m pytest" in line)
    assert "scripts/test_bump_released_crate_versions.py" in command.split()
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)


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


def test_obsolete_mssql_python_linux_build_is_removed():
    stages = load_template("validation-stages.yml")["stages"]
    stage = next(stage for stage in stages if stage["stage"] == "Build_mssql_python")
    jobs = {job["job"]: job for job in stage["jobs"]}
    assert "Build_mssql_python_Linux" not in jobs
    assert jobs["Build_mssql_python_MacOS"]["steps"] == [
        {"template": "test-mssql-python-macos-template.yml"}
    ]
    assert jobs["Test_mssql_python_on_mssql_odbc"]["steps"] == [
        {"template": "test-mssql-python-odbc-template.yml"}
    ]
    removed_template = "test-mssql-python-template.yml"
    assert not (_TEMPLATES / removed_template).exists()
    for path in _TEMPLATES.glob("*.yml"):
        assert removed_template not in path.read_text(encoding="utf-8"), path.name


@pytest.mark.parametrize(
    "template",
    ["test-mssql-python-macos-template.yml", "test-mssql-python-odbc-template.yml"],
)
def test_cross_repo_jobs_share_the_revision_checkout(template):
    steps = load_template(template)["steps"]
    clone = next(step for step in steps if step.get("displayName") == "Clone mssql-python")
    assert "bash .pipeline/scripts/clone-mssql-python.sh" in clone["script"]
    assert "env" not in clone
    assert "continueOnError" not in clone
    text = (_TEMPLATES / template).read_text(encoding="utf-8")
    assert "git clone" not in text
    assert "mssql-python-branch" not in text


def test_mssql_python_macos_failures_are_advisory():
    steps = load_template("test-mssql-python-macos-template.yml")["steps"]
    run = next(step for step in steps if step.get("displayName") == "Run mssql-python tests")
    assert "continueOnError" not in run
    assert "task.complete result=SucceededWithIssues" in run["script"]
    publish = next(step for step in steps if step.get("task") == "PublishTestResults@2")
    assert publish["condition"] == "succeededOrFailed()"
    assert publish["inputs"]["failTaskOnFailedTests"] is False
    assert publish["inputs"]["failTaskOnMissingResultsFile"] is True


def test_cross_repo_validation_is_not_path_filtered_or_optional():
    pipeline = yaml.safe_load(
        (_ROOT / ".pipeline" / "validation-pipeline.yml").read_text(encoding="utf-8")
    )
    # Server-side PR filters also need to include .pipeline/ (see scripts/README.md).
    assert "pr" not in pipeline
    stages = load_template("validation-stages.yml")["stages"]
    stage = next(stage for stage in stages if stage["stage"] == "Build_mssql_python")
    assert stage["dependsOn"] == ["EvaluateDuplicate"]
    assert stage["condition"] == (
        "and(not(canceled()), eq(variables['Build.Reason'], 'PullRequest'), "
        "eq('${{ parameters.RunFuzz }}', 'false'), "
        "eq('${{ parameters.RunLongHaul }}', 'false'), "
        "ne(dependencies.EvaluateDuplicate.outputs['Evaluate.SetDuplicateState.skipDuplicate'], 'true'))"
    )
    for job in stage["jobs"]:
        assert "condition" not in job
        assert "continueOnError" not in job
    build = next(stage for stage in stages if stage["stage"] == "Build")
    linux = next(job for job in build["jobs"] if job.get("job") == "Build_Linux")
    assert any(
        "unittest discover -s .pipeline/scripts" in step.get("bash", "")
        for step in linux["steps"]
    )


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


def test_mssql_python_odbc_failures_are_advisory():
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
    assert "task.complete result=SucceededWithIssues" in test_step["script"]
    publish = next(step for step in steps if step.get("task") == "PublishTestResults@2")
    assert publish["inputs"]["failTaskOnFailedTests"] is False

    # A docker-exec launch failure (125/126/127), or the runner's own exit 2
    # for a broken harness, both mean the tests said nothing about the driver,
    # so each must still fail the job (exit "$rc") through a branch that says
    # so rather than reporting it as a driver test failure.
    assert re.search(r"\b2\)", test_step["script"])
    assert re.search(r"125\|126\|127\)", test_step["script"])
    assert "exit \"$rc\"" in test_step["script"].rsplit("esac", 1)[-1]

    # Harness failures must not be masked by a job-level continueOnError.
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


@pytest.mark.skipif(shutil.which("bash") is None, reason="Bash is required")
@pytest.mark.parametrize(
    ("template", "display_name", "command"),
    [
        ("test-mssql-python-macos-template.yml", "Run mssql-python tests", "pytest"),
        (
            "test-mssql-python-odbc-template.yml",
            "Run mssql-python tests against mssql-odbc",
            "docker",
        ),
    ],
)
@pytest.mark.parametrize("exit_code", [0, 1, 2, 3, 4, 5, 125, 126, 127, 137])
def test_python_pipeline_test_exit_codes(template, display_name, command, exit_code):
    step = next(
        step for step in load_template(template)["steps"]
        if step.get("displayName") == display_name
    )
    script = step["script"].replace("$(Build.SourcesDirectory)", "/workspace")
    script = re.sub(r"\$\{\{.*?\}\}", "10m", script)
    result = subprocess.run(
        ["bash", "-c", f"{command}() {{ return {exit_code}; }}\n{script}"],
        capture_output=True, text=True,
    )
    assert result.returncode == (0 if exit_code == 1 else exit_code), result.stderr
    assert ("task.logissue type=warning" in result.stdout) == (exit_code == 1)
    assert ("task.complete result=SucceededWithIssues;" in result.stdout) == (exit_code == 1)


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
    toolchain = build["variables"]["miriToolchain"]
    assert re.fullmatch(r"nightly-\d{4}-\d{2}-\d{2}", toolchain)
    readme = (_ROOT / "mssql-odbc" / "README.md").read_text(encoding="utf-8")
    assert set(re.findall(r"nightly-\d{4}-\d{2}-\d{2}", readme)) == {toolchain}, (
        "Update the README's Miri version when changing miriToolchain"
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
        assert "rustup toolchain install $(miriToolchain)" in command
        assert "--component miri,rust-src" in command
        assert f"cargo +$(miriToolchain) miri setup --target {target}" in command
        assert "cargo +$(miriToolchain) miri nextest run" in command
        assert command.count("$(miriToolchain)") == 3
        assert "nightly-" not in command
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


def test_shared_miri_filter_does_not_require_the_odbc_package():
    config = (_ROOT / ".config" / "nextest.toml").read_text(encoding="utf-8")
    profile = config.split("[profile.miri-odbc]\n", 1)[1].split("\n[", 1)[0]
    assert (
        "default-filter = 'test(::memory_safety::) | "
        "test(conversion::param_buffer::tests::misaligned_)'"
    ) in profile


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


def test_non_windows_format_installs_rustfmt_first():
    steps = load_template("build-template.yml")["steps"]
    non_windows = next(
        group for group in steps
        if "${{ if ne(parameters.osType, 'Windows') }}" in group
    )
    branch = non_windows["${{ if ne(parameters.osType, 'Windows') }}"]
    install = next(step for step in branch if step.get("displayName") == "Install Rustfmt")
    fmt = next(
        step for step in branch
        if step.get("displayName") == "Check Format (workspace + mssql-py-core)"
    )
    assert branch.index(install) < branch.index(fmt)
    assert install["script"] == "rustup component add rustfmt"
    assert install["condition"] == fmt["condition"]
    assert install["retryCountOnTaskFailure"] == 3

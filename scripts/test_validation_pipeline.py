# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Regression tests for validation pipeline builds, tests, and artifacts."""

import importlib.util
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace
import xml.etree.ElementTree as ET

import pytest
import yaml

_ROOT = Path(__file__).parents[1]
_TEMPLATES = _ROOT / ".pipeline" / "templates"
_NON_PR = "and(succeeded(), ne(variables['Build.Reason'], 'PullRequest'))"
_PR = "and(succeeded(), eq(variables['Build.Reason'], 'PullRequest'))"
_BASH = shutil.which("bash")
_PREPARE = _ROOT / ".pipeline" / "scripts" / "prepare-mssql-python.py"
_SPEC = importlib.util.spec_from_file_location("prepare_mssql_python", _PREPARE)
assert _SPEC and _SPEC.loader
prepare_python = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(prepare_python)


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
def test_cross_repo_jobs_share_the_pinned_checkout(template):
    steps = load_template(template)["steps"]
    clone = next(step for step in steps if step.get("displayName") == "Clone mssql-python")
    assert "bash .pipeline/scripts/clone-mssql-python.sh" in clone["script"]
    assert "env" not in clone
    assert "continueOnError" not in clone
    text = (_TEMPLATES / template).read_text(encoding="utf-8")
    assert "git clone" not in text
    assert "mssql-python-branch" not in text


def test_mssql_python_macos_failures_are_advisory_only_in_ci():
    steps = load_template("test-mssql-python-macos-template.yml")["steps"]
    run = next(step for step in steps if step.get("displayName") == "Run mssql-python tests")
    assert "continueOnError" not in run
    assert "task.complete result=SucceededWithIssues" in run["script"]
    assert run["condition"] == "and(succeeded(), eq(variables['mssqlPythonReady'], 'true'))"
    publish = next(
        step for step in steps
        if step.get("displayName") == "Publish mssql-python macOS test results"
    )
    assert publish["condition"] == (
        "and(succeededOrFailed(), eq(variables['mssqlPythonReady'], 'true'))"
    )
    assert publish["inputs"]["failTaskOnFailedTests"] is False
    assert publish["inputs"]["failTaskOnMissingResultsFile"] is True


def test_mssql_python_macos_setup_results_survive_verification_failure():
    steps = load_template("test-mssql-python-macos-template.yml")["steps"]
    build = next(
        step for step in steps
        if step.get("displayName") == "Build ddbc_bindings and mssql-py-core"
    )
    setup = next(
        step for step in steps
        if step.get("displayName") == "Prepare and verify mssql-python runtime"
    )
    publish = next(
        step for step in steps
        if step.get("displayName") == "Publish mssql-python macOS setup results"
    )
    run = next(step for step in steps if step.get("displayName") == "Run mssql-python tests")
    assert steps.index(build) < steps.index(setup) < steps.index(publish) < steps.index(run)
    for step in (build, setup):
        assert not step.get("continueOnError", False)
        assert step.get("condition", "succeeded()") == "succeeded()"
    assert publish["condition"] == "succeededOrFailed()"
    assert publish["inputs"]["testResultsFiles"] == "**/mssql-python-macos-setup-results.xml"
    assert publish["inputs"]["failTaskOnFailedTests"] is False
    assert publish["inputs"]["failTaskOnMissingResultsFile"] is True


@pytest.mark.skipif(_BASH is None, reason="Bash is required")
@pytest.mark.parametrize("rust_exit", [0, 1, 127])
def test_macos_build_installs_local_rust_before_upstream_setup(tmp_path, rust_exit):
    steps = load_template("test-mssql-python-macos-template.yml")["steps"]
    names = {"Build ddbc_bindings and mssql-py-core", "Prepare and verify mssql-python runtime"}
    scripts = [step["script"] for step in steps if step.get("displayName") in names]
    source = tmp_path / "rust"
    (source / "mssql-py-core").mkdir(parents=True)
    bindings = tmp_path / "mssql-python" / "mssql_python" / "pybind"
    bindings.mkdir(parents=True)
    (bindings / "build.sh").write_text('echo bindings >> "$LOG"\n', newline="\n")
    script = f"""
    ROOT="$PWD"
    export LOG="$ROOT/commands"
    python() {{
        printf '%s\\t' python "$@" >> "$LOG"
        printf '\\n' >> "$LOG"
    }}
    maturin() {{
        echo maturin >> "$LOG"
        mkdir dist
        touch dist/local-rust.whl
        return {rust_exit}
    }}
    """
    # Each Azure script starts in the sources directory, in a fresh shell.
    script += "\nset -e\n" + "\n".join(
        '(cd "$ROOT/rust";\n'
        + text.replace("$(Build.SourcesDirectory)", "$ROOT/rust")
        + "\n)"
        for text in scripts
    )
    result = subprocess.run(
        [_BASH, "-s"], input=script, cwd=tmp_path, capture_output=True, text=True,
    )
    assert result.returncode == rust_exit, result.stdout + result.stderr
    commands = (tmp_path / "commands").read_text().splitlines()
    if rust_exit:
        assert commands[-1] == "maturin"
        return
    install = next(i for i, command in enumerate(commands) if "dist/local-rust.whl" in command)
    setup = commands[-1].split("\t")[:-1]
    assert commands.index("bindings") < commands.index("maturin") < install < len(commands) - 1
    assert setup[:3] == ["python", ".pipeline/scripts/prepare-mssql-python.py", "--upstream"]
    assert Path(setup[3]).name == "mssql-python"
    assert setup[4] == "--results"
    assert Path(setup[5]).name == "mssql-python-macos-setup-results.xml"


@pytest.mark.parametrize("count", [0, 1, 2])
def test_odbc_wheel_selection_executes_all_cardinalities(tmp_path, count):
    (tmp_path / "unrelated-1.0.whl").touch()
    for index in range(count):
        (tmp_path / f"mssql_python_odbc-{index}.whl").touch()
    if count == 1:
        assert prepare_python.select_odbc_wheel(tmp_path).name == "mssql_python_odbc-0.whl"
    else:
        with pytest.raises(prepare_python.UpstreamContractError, match=f"found {count}"):
            prepare_python.select_odbc_wheel(tmp_path)


def test_upstream_setup_builds_installs_and_verifies_in_order(tmp_path, monkeypatch):
    tmp_path = tmp_path / "upstream with spaces; literal"
    tmp_path.mkdir()
    (tmp_path / "setup_odbc.py").touch()
    calls = []

    def run(arguments, **kwargs):
        assert not kwargs.get("shell", False)
        calls.append(arguments[1:])
        if arguments[1] == "setup_odbc.py":
            assert kwargs == {"cwd": tmp_path, "check": True}
            (Path(arguments[-1]) / "mssql_python_odbc-1.0.whl").touch()
        return subprocess.CompletedProcess(arguments, 0)

    monkeypatch.setattr(prepare_python.subprocess, "run", run)
    monkeypatch.setattr(prepare_python, "check_runtime_dependencies", lambda: calls.append(["metadata"]))
    prepare_python.prepare(tmp_path)
    assert calls[0][:3] == ["setup_odbc.py", "bdist_wheel", "--dist-dir"]
    assert calls[1][:4] == ["-m", "pip", "install", "--no-deps"]
    assert Path(calls[1][4]).name == "mssql_python_odbc-1.0.whl"
    assert calls[2] == ["-m", "pip", "install", "--no-deps", "-e", str(tmp_path)]
    assert calls[3] == ["metadata"]
    assert calls[4] == [str(_PREPARE.resolve()), "--verify-provider"]
    assert len(calls) == 5


@pytest.mark.parametrize("reason", ["PullRequest", "IndividualCI"])
@pytest.mark.parametrize("failed_command", [0, 1, 2, 3])
def test_setup_subprocess_failures_are_not_advisory(tmp_path, monkeypatch, reason, failed_command):
    (tmp_path / "setup_odbc.py").touch()
    monkeypatch.setenv("BUILD_REASON", reason)
    calls = []

    def run(arguments, **kwargs):
        index = len(calls)
        calls.append(arguments)
        if index == failed_command:
            if kwargs["check"]:
                raise subprocess.CalledProcessError(1, arguments)
            return subprocess.CompletedProcess(arguments, 139)
        if index == 0:
            (Path(arguments[-1]) / "mssql_python_odbc-1.0.whl").touch()
        return subprocess.CompletedProcess(arguments, 0)

    monkeypatch.setattr(prepare_python.subprocess, "run", run)
    monkeypatch.setattr(prepare_python, "check_runtime_dependencies", lambda: None)
    report = tmp_path / "setup.xml"
    with pytest.raises(subprocess.CalledProcessError):
        prepare_python.main(["--upstream", str(tmp_path), "--results", str(report)])
    assert len(calls) == failed_command + 1
    assert ET.parse(report).getroot().get("failures") == "1"


@pytest.mark.parametrize("reason", [None, "IndividualCI"])
def test_provider_subprocess_drift_reaches_setup_policy(tmp_path, monkeypatch, reason, capsys):
    (tmp_path / "setup_odbc.py").touch()
    if reason is None:
        monkeypatch.delenv("BUILD_REASON", raising=False)
    else:
        monkeypatch.setenv("BUILD_REASON", reason)

    def run(arguments, **kwargs):
        if arguments[1] == "setup_odbc.py":
            (Path(arguments[-1]) / "mssql_python_odbc-1.0.whl").touch()
        code = prepare_python.CONTRACT_EXIT if "--verify-provider" in arguments else 0
        return subprocess.CompletedProcess(arguments, code)

    monkeypatch.setattr(prepare_python.subprocess, "run", run)
    monkeypatch.setattr(prepare_python, "check_runtime_dependencies", lambda: None)
    report = tmp_path / "setup.xml"
    result = prepare_python.main(["--upstream", str(tmp_path), "--results", str(report)])
    advisory = reason is not None
    assert result == int(not advisory)
    assert "variable=mssqlPythonReady]true" not in capsys.readouterr().out
    suite = ET.parse(report).getroot()
    assert suite.get("skipped") == str(int(advisory))
    assert suite.get("failures") == str(int(not advisory))


@pytest.mark.parametrize(
    ("provider", "diagnostic"),
    [
        ("{'id': 'msodbcsql18', 'driver_path': DRIVER}", None),
        ("{'id': 'other', 'driver_path': DRIVER}", "Expected provider id"),
        ("{'driver_path': DRIVER}", "Expected provider id"),
        ("{'id': 'msodbcsql18'}", "driver_path"),
        ("{'id': 'msodbcsql18', 'driver_path': ''}", "driver_path"),
        ("{'id': 'msodbcsql18', 'driver_path': DRIVER + '.missing'}", "driver_path"),
        ("{'id': 'msodbcsql18', 'driver_path': str(Path(DRIVER).parent)}", "driver_path"),
        ("{'id': 'msodbcsql18', 'driver_path': 1}", "driver_path"),
        ("None", "Expected provider id"),
    ],
)
def test_provider_verification_in_fresh_interpreter(tmp_path, provider, diagnostic):
    driver = tmp_path / "test driver.dylib"
    driver.touch()
    (tmp_path / "mssql_python.py").write_text(
        f"from pathlib import Path\nDRIVER = {str(driver)!r}\n"
        f"def get_native_provider_info():\n    return {provider}\n",
        encoding="utf-8",
    )
    result = subprocess.run(
        [sys.executable, str(_PREPARE), "--verify-provider"],
        env={**os.environ, "PYTHONPATH": str(tmp_path)},
        capture_output=True, text=True,
    )
    assert result.returncode == (prepare_python.CONTRACT_EXIT if diagnostic else 0), result.stderr
    assert (diagnostic or "ODBC provider:") in result.stdout + result.stderr


@pytest.mark.parametrize(
    ("body", "contract_error"),
    [
        ("", True),
        ("get_native_provider_info = None", True),
        ("raise ModuleNotFoundError('package renamed', name='mssql_python')", True),
        ("raise ModuleNotFoundError('dependency missing', name='dependency')", False),
        ("raise ImportError('ddbc_bindings ABI/link failure')", False),
        ("def get_native_provider_info(required): pass", True),
        ("def get_native_provider_info(): raise AttributeError('unexpected failure')", False),
        ("def get_native_provider_info(): raise KeyError('unexpected failure')", False),
        ("def get_native_provider_info(): raise TypeError('unexpected failure')", False),
        ("def get_native_provider_info(): raise ValueError('unexpected failure')", False),
        ("def get_native_provider_info(): raise ImportError('unexpected failure')", False),
        ("def get_native_provider_info(): raise RuntimeError('unexpected failure')", False),
        ("import os\nos._exit(139)", False),
    ],
)
def test_provider_interface_drift_does_not_hide_unexpected_errors(tmp_path, body, contract_error):
    (tmp_path / "mssql_python.py").write_text(body, encoding="utf-8")
    result = subprocess.run(
        [sys.executable, str(_PREPARE), "--verify-provider"],
        env={**os.environ, "PYTHONPATH": str(tmp_path)}, capture_output=True, text=True,
    )
    assert result.returncode != 0
    assert (result.returncode == prepare_python.CONTRACT_EXIT) == contract_error


@pytest.fixture
def runtime_metadata(monkeypatch):
    requirements = ["mssql_python_rs==9.9", "MSSQL.Python.ODBC==9.9", "Azure_Identity>=1.12"]
    versions = {"mssql-python-rs": "0.1.dev1", "mssql-python-odbc": "0.1.dev1", "azure-identity": "1.12"}
    monkeypatch.setattr(
        prepare_python.metadata, "distribution", lambda _: SimpleNamespace(requires=requirements),
    )

    def version(name):
        if name not in versions:
            raise prepare_python.metadata.PackageNotFoundError(name)
        return versions[name]

    monkeypatch.setattr(prepare_python.metadata, "version", version)
    monkeypatch.setattr(prepare_python.metadata, "requires", lambda _: [])
    monkeypatch.setattr(
        prepare_python.metadata, "metadata",
        lambda _: SimpleNamespace(get_all=lambda key, default: ["feature"]),
    )
    return requirements, versions


def test_runtime_metadata_respects_markers_and_local_native_ownership(runtime_metadata, capsys):
    requirements, _ = runtime_metadata
    requirements.extend(['not-installed; python_version < "2"', 'optional; extra == "pyarrow"'])
    prepare_python.check_runtime_dependencies()
    assert capsys.readouterr().out.count("locally built; ignoring upstream") == 2


def test_installed_prerelease_can_satisfy_runtime_requirement(runtime_metadata):
    _, versions = runtime_metadata
    versions["azure-identity"] = "1.13.dev1"
    prepare_python.check_runtime_dependencies()


@pytest.mark.parametrize(
    "dependency",
    ["new-runtime>=1", "azure-identity>=2", "new-runtime[feature]>=1", "new-runtime; python_version >= '3'"],
)
def test_new_or_incompatible_runtime_dependency_fails_setup(runtime_metadata, dependency):
    requirements, _ = runtime_metadata
    requirements.append(dependency)
    with pytest.raises(RuntimeError, match="Unsatisfied.*Update the setup dependencies"):
        prepare_python.check_runtime_dependencies()


def test_native_distribution_rename_is_contract_drift(runtime_metadata):
    requirements, _ = runtime_metadata
    requirements[0] = "new-rust-distribution==9.9"
    with pytest.raises(prepare_python.UpstreamContractError, match="no longer declares.*mssql-python-rs"):
        prepare_python.check_runtime_dependencies()


def test_missing_native_install_is_not_contract_drift(runtime_metadata):
    _, versions = runtime_metadata
    del versions["mssql-python-rs"]
    with pytest.raises(RuntimeError, match="mssql_python_rs==9.9: not installed") as error:
        prepare_python.check_runtime_dependencies()
    assert not isinstance(error.value, prepare_python.UpstreamContractError)


def test_missing_upstream_distribution_is_contract_drift(monkeypatch):
    def distribution(_):
        raise prepare_python.metadata.PackageNotFoundError("mssql-python")

    monkeypatch.setattr(prepare_python.metadata, "distribution", distribution)
    with pytest.raises(prepare_python.UpstreamContractError, match="did not provide.*mssql-python"):
        prepare_python.check_runtime_dependencies()


def test_malformed_metadata_remains_blocking(runtime_metadata):
    requirements, _ = runtime_metadata
    requirements.append("--not-a-requirement")
    with pytest.raises(ValueError):
        prepare_python.check_runtime_dependencies()


def test_unverifiable_url_dependency_fails_setup(runtime_metadata):
    requirements, versions = runtime_metadata
    requirements.append("new-runtime @ https://example.invalid/new-runtime.whl")
    versions["new-runtime"] = "1.0"
    with pytest.raises(RuntimeError, match="cannot verify a direct-URL runtime dependency"):
        prepare_python.check_runtime_dependencies()


@pytest.mark.parametrize("extra", ["missing", "feature", "FEATURE", "feature_name"])
def test_runtime_extras_must_be_declared(runtime_metadata, monkeypatch, extra):
    requirements, versions = runtime_metadata
    requirements.append(f"new-runtime[{extra}]>=1")
    versions["new-runtime"] = "1.0"
    monkeypatch.setattr(
        prepare_python.metadata, "metadata",
        lambda _: SimpleNamespace(get_all=lambda key, default: ["feature", "feature-name"]),
    )
    if extra == "missing":
        with pytest.raises(RuntimeError, match="does not declare extras.*missing"):
            prepare_python.check_runtime_dependencies()
    else:
        prepare_python.check_runtime_dependencies()


@pytest.mark.parametrize("extra_installed", [False, True])
@pytest.mark.parametrize("base_installed", [False, True])
def test_required_extras_and_transitive_dependencies_are_checked(
    runtime_metadata, monkeypatch, extra_installed, base_installed,
):
    requirements, versions = runtime_metadata
    requirements.append("new-runtime[feature]>=1")
    versions["new-runtime"] = "1.0"
    if extra_installed:
        versions["extra-runtime"] = "2.0"
    if base_installed:
        versions["base-runtime"] = "1.0"
    nested = {
        "new-runtime": [
            "extra-runtime>=2; extra == 'feature'",
            "base-runtime>=1; extra != 'feature'",
            "unused; extra == 'unused'",
        ],
        "extra-runtime": ["new-runtime[feature]>=1"],
    }
    monkeypatch.setattr(prepare_python.metadata, "requires", lambda name: nested.get(name, []))
    if extra_installed and base_installed:
        prepare_python.check_runtime_dependencies()
    else:
        missing = "extra-runtime>=2" if not extra_installed else "base-runtime>=1"
        with pytest.raises(RuntimeError, match=f"{missing}.*not installed"):
            prepare_python.check_runtime_dependencies()


@pytest.mark.parametrize("reason", [None, "", "PullRequest", "IndividualCI", "BatchedCI", "Manual", "Schedule"])
@pytest.mark.parametrize("drift", [False, True])
def test_setup_policy_and_junit_results(tmp_path, monkeypatch, capsys, reason, drift):
    if reason is None:
        monkeypatch.delenv("BUILD_REASON", raising=False)
    else:
        monkeypatch.setenv("BUILD_REASON", reason)

    def prepare(_):
        if drift:
            raise prepare_python.UpstreamContractError("provider <contract> changed")

    monkeypatch.setattr(prepare_python, "prepare", prepare)
    report = tmp_path / "setup.xml"
    result = prepare_python.main(["--upstream", str(tmp_path), "--results", str(report)])
    output = capsys.readouterr()
    advisory = drift and reason not in (None, "", "PullRequest")
    assert result == int(drift and not advisory)
    assert ("task.logissue type=warning" in output.out) == advisory
    assert ("task.complete result=SucceededWithIssues" in output.out) == advisory
    assert ("variable=mssqlPythonReady]true" in output.out) == (not drift)
    suite = ET.parse(report).getroot()
    assert suite.get("failures") == str(int(drift and not advisory))
    assert suite.get("skipped") == str(int(advisory))
    if drift:
        assert "provider <contract> changed" in suite.find("testcase")[0].get("message")


@pytest.mark.parametrize("reason", ["PullRequest", "IndividualCI"])
@pytest.mark.parametrize(
    "error",
    [FileNotFoundError("missing checkout"), subprocess.CalledProcessError(1, ["build"]),
     RuntimeError("dependency failure"), OSError("disk full")],
)
def test_setup_errors_remain_blocking_and_publish_failure(tmp_path, monkeypatch, capsys, reason, error):
    monkeypatch.setenv("BUILD_REASON", reason)

    def prepare(_):
        raise error

    monkeypatch.setattr(prepare_python, "prepare", prepare)
    report = tmp_path / "setup.xml"
    with pytest.raises(type(error)):
        prepare_python.main(["--upstream", str(tmp_path), "--results", str(report)])
    assert "task.complete" not in capsys.readouterr().out
    assert ET.parse(report).getroot().get("failures") == "1"


@pytest.mark.parametrize("drift", [False, True])
def test_missing_setup_result_cannot_report_success(tmp_path, monkeypatch, capsys, drift):
    monkeypatch.setenv("BUILD_REASON", "IndividualCI")

    def prepare(_):
        if drift:
            raise prepare_python.UpstreamContractError("provider changed")

    monkeypatch.setattr(prepare_python, "prepare", prepare)
    report = tmp_path / "missing-directory" / "setup.xml"
    with pytest.raises(FileNotFoundError):
        prepare_python.main(["--upstream", str(tmp_path), "--results", str(report)])
    output = capsys.readouterr().out
    assert "variable=mssqlPythonReady]true" not in output
    assert "task.complete" not in output


def test_missing_packaging_entrypoint_is_recognized_drift(tmp_path):
    with pytest.raises(prepare_python.UpstreamContractError, match="setup_odbc.py"):
        prepare_python.prepare(tmp_path)
    with pytest.raises(FileNotFoundError, match="checkout"):
        prepare_python.prepare(tmp_path / "missing")


def test_pin_validation_is_not_path_filtered_or_optional():
    pipeline = yaml.safe_load(
        (_ROOT / ".pipeline" / "validation-pipeline.yml").read_text(encoding="utf-8")
    )
    # Server-side PR filters also need to include .pipeline/ (see scripts/README.md).
    assert "pr" not in pipeline
    stages = load_template("validation-stages.yml")["stages"]
    stage = next(stage for stage in stages if stage["stage"] == "Build_mssql_python")
    assert stage["dependsOn"] == ["EvaluateDuplicate"]
    assert stage["condition"] == (
        "and(not(canceled()), "
        "eq('${{ parameters.RunFuzz }}', 'false'), "
        "eq('${{ parameters.RunLongHaul }}', 'false'), "
        "or(ne(variables['Build.Reason'], 'PullRequest'), "
        "ne(dependencies.EvaluateDuplicate.outputs['Evaluate.SetDuplicateState.skipDuplicate'], 'true')))"
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


def test_mssql_python_odbc_failures_are_advisory_only_in_ci():
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


@pytest.mark.skipif(_BASH is None, reason="Bash is required")
@pytest.mark.parametrize(
    ("pytest_exit", "runner_exit"),
    [(0, 0), (1, 1), (2, 1), (3, 2), (4, 2), (5, 0),
     (124, 1), (125, 2), (126, 2), (127, 2), (137, 1), (139, 1)],
)
def test_odbc_runner_distinguishes_harness_errors(tmp_path, pytest_exit, runner_exit):
    tests = tmp_path / "tests"
    tests.mkdir()
    (tests / "test_pass.py").touch()
    (tests / "test_result.py").touch()
    runner = _ROOT / ".pipeline" / "scripts" / "run-mssql-python-odbc-tests.sh"
    (tmp_path / "runner.sh").write_text(runner.read_text(encoding="utf-8"), newline="\n")
    script = f"""
    python() {{ return 0; }}
    timeout() {{
        for arg in "$@"; do
            case "$arg" in
                tests/test_pass.py) return 0 ;;
                tests/test_result.py) return {pytest_exit} ;;
            esac
        done
        echo "Unexpected timeout arguments: $*" >&2
        return 125
    }}
    MSSQL_PYTHON_DIR="$2" TEST_RESULTS_DIR="$2/test-results" \
        PYTEST_FILE_TIMEOUT=10s PYTEST_TOTAL_BUDGET=120s source "$1"
    """
    script_path = tmp_path / "run-test.sh"
    script_path.write_text(script, encoding="utf-8", newline="\n")
    result = subprocess.run(
        [_BASH, script_path.name, "./runner.sh", "."],
        cwd=tmp_path, capture_output=True, text=True,
    )
    assert result.returncode == runner_exit, result.stdout + result.stderr
    assert "Unexpected timeout arguments" not in result.stderr
    assert f"passed: {2 if pytest_exit == 0 else 1} |" in result.stdout
    assert f"harness errors: {int(runner_exit == 2)}" in result.stdout
    assert len(list((tmp_path / "test-results").glob("*.xml"))) == 2


@pytest.mark.skipif(_BASH is None, reason="Bash is required")
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
@pytest.mark.parametrize("reason", ["PullRequest", "IndividualCI", "BatchedCI", "Manual", "Schedule", ""])
def test_python_pipeline_test_exit_codes(tmp_path, template, display_name, command, exit_code, reason):
    step = next(
        step for step in load_template(template)["steps"]
        if step.get("displayName") == display_name
    )
    script = step["script"].replace("$(Build.SourcesDirectory)", "/workspace")
    script = re.sub(r"\$\{\{.*?\}\}", "10m", script)
    script_path = tmp_path / "run-step.sh"
    script_path.write_text(
        f"BUILD_REASON='{reason}'\n{command}() {{ return {exit_code}; }}\n{script}",
        encoding="utf-8", newline="\n",
    )
    result = subprocess.run(
        [_BASH, script_path.name],
        cwd=tmp_path, capture_output=True, text=True,
    )
    advisory = exit_code == 1 and reason not in ("PullRequest", "")
    assert result.returncode == (0 if advisory else exit_code), result.stderr
    assert ("task.logissue type=warning" in result.stdout) == advisory
    assert ("task.complete result=SucceededWithIssues;" in result.stdout) == advisory


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

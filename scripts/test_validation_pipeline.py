# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Regression tests for validation pipeline builds, tests, and artifacts."""

import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest
import yaml

from test_release_pipeline import evaluate

_ROOT = Path(__file__).parents[1]
_TEMPLATES = _ROOT / ".pipeline" / "templates"
_NON_PR = "and(succeeded(), ne(variables['Build.Reason'], 'PullRequest'))"
_PR = "and(succeeded(), eq(variables['Build.Reason'], 'PullRequest'))"
_BASH = shutil.which("bash")
_VERIFY = _ROOT / ".pipeline" / "scripts" / "verify-mssql-python.py"
_SPEC = importlib.util.spec_from_file_location("verify_mssql_python", _VERIFY)
assert _SPEC and _SPEC.loader
verify_python = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(verify_python)


def load_template(name):
    return yaml.safe_load((_TEMPLATES / name).read_text(encoding="utf-8"))


@pytest.fixture
def stress_pipeline():
    return yaml.safe_load(
        (_ROOT / ".pipeline" / "odbc-stress-pipeline.yml").read_text(encoding="utf-8")
    )


@pytest.fixture
def windows_stress_job(stress_pipeline):
    stage = next(
        stage for stage in stress_pipeline["stages"] if stage["stage"] == "Stress_Windows"
    )
    assert stage["dependsOn"] == []
    return next(job for job in stage["jobs"] if job["job"] == "Stress_Windows")


@pytest.mark.parametrize(
    ("profile", "schedule", "expected"),
    [
        ("auto", None, 120),
        ("auto", "", 120),
        ("auto", "Nightly ODBC stress", 120),
        ("auto", "Weekly ODBC stress", 240),
        ("weekly", None, 240),
        ("weekly", "Nightly ODBC stress", 240),
        ("nightly", "Weekly ODBC stress", 120),
    ],
)
def test_stress_job_timeout_follows_profile(stress_pipeline, profile, schedule, expected):
    variables = stress_pipeline["variables"]
    conditional = next(key for key in variables if key.startswith("${{ if "))
    condition = conditional.removeprefix("${{ if ").removesuffix(" }}")
    weekly = evaluate(
        condition, {"profile": profile},
        variables={"Build.CronSchedule.DisplayName": schedule},
    )
    selected = variables[conditional if weekly else "${{ else }}"]
    assert selected["odbcMtJobTimeoutInMinutes"] == expected
    timeout = "${{ variables.odbcMtJobTimeoutInMinutes }}"
    windows = next(
        stage for stage in stress_pipeline["stages"] if stage["stage"] == "Stress_Windows"
    )
    execution = next(job for job in windows["jobs"] if job["job"] == "Stress_Windows")
    assert execution["timeoutInMinutes"] == timeout
    linux = next(
        stage for stage in stress_pipeline["stages"] if stage["stage"] == "Stress_Linux"
    )
    assert len(linux["jobs"]) == 2
    for job in linux["jobs"]:
        assert job["parameters"]["timeoutInMinutes"] == timeout
        assert len(job["parameters"]["matrix"]) == 1
    build = next(
        stage for stage in stress_pipeline["stages"] if stage["stage"] == "Build_Linux_Drops"
    )
    assert build["jobs"][0]["timeoutInMinutes"] == 90


def test_windows_stress_matrix_preserves_profiles_and_parity(windows_stress_job):
    matrix = windows_stress_job["strategy"]["matrix"]
    assert set(matrix) == {"Fetch", "Parameters", "Metadata", "Other"}
    assert {group["suiteGroup"] for group in matrix.values()} == set(matrix)
    assert "maxParallel" not in windows_stress_job["strategy"]
    # Matrix variables are not expanded in the job name; Azure appends the leg.
    assert "$(" not in windows_stress_job["displayName"]
    assert windows_stress_job["timeoutInMinutes"] == "${{ variables.odbcMtJobTimeoutInMinutes }}"
    step = next(step for step in windows_stress_job["steps"] if "pwsh" in step)
    command = step["pwsh"]
    assert "-CompareWithMsodbcsql" in command
    assert "-SkipBuild" in command
    assert "-MtProfile '$(odbcMtProfile)'" in command
    assert '-FilterTests "$env:FILTERTESTS" -ExcludeTests "$env:EXCLUDETESTS"' in command
    # Azure leaves $(name) literal when a matrix value is empty.
    for name in {key for group in matrix.values() for key, value in group.items() if not value}:
        assert f"$({name})" not in command


def test_windows_stress_builds_once_and_shares_attempt_scoped_artifact(
    stress_pipeline, windows_stress_job
):
    stage = next(s for s in stress_pipeline["stages"] if s["stage"] == "Stress_Windows")
    build = next(j for j in stage["jobs"] if j["job"] == "Build_Odbc_Windows")
    assert build["pool"] == windows_stress_job["pool"]
    assert "strategy" not in build
    assert build["timeoutInMinutes"] == 120
    assert windows_stress_job["dependsOn"] == build["job"]
    step = next(s for s in build["steps"] if s.get("name") == "buildDrop")
    assert "-BuildOnly" in step["pwsh"]
    assert "$buildJobs = [Math]::Min(4, [Environment]::ProcessorCount)" in step["pwsh"]
    assert "-BuildJobs $buildJobs" in step["pwsh"]
    assert '-OutDir "$(Build.ArtifactStagingDirectory)\\odbc-e2e"' in step["pwsh"]
    assert "OdbcStress_Windows_$(System.JobAttempt)" in step["pwsh"]
    assert "isOutput=true" in step["pwsh"]
    publish = next(s for s in build["steps"] if s.get("task") == "PublishPipelineArtifact@1")
    assert publish["inputs"]["artifact"] == "$(buildDrop.artifactName)"
    assert publish.get("condition", "succeeded()") == "succeeded()"
    assert windows_stress_job["variables"]["odbcDropArtifact"] == (
        "$[ dependencies.Build_Odbc_Windows.outputs['buildDrop.artifactName'] ]"
    )
    download = next(
        s for s in windows_stress_job["steps"]
        if s.get("task") == "DownloadPipelineArtifact@2"
    )
    assert download["inputs"] == {
        "buildType": "current",
        "artifactName": "$(odbcDropArtifact)",
        "targetPath": "$(Build.SourcesDirectory)\\mssql-odbc\\tests\\e2e",
    }
    deps = next(
        s for s in windows_stress_job["steps"]
        if s.get("template") == "templates/install-dependencies.yml"
    )
    assert deps["parameters"]["skipRustupInstall"] is True
    assert deps["parameters"]["enableJsDeps"] is False
    assert not any(
        s.get("template") == "templates/cargo-authenticate-template.yml"
        for s in windows_stress_job["steps"]
    )


_WINDOWS_E2E = _ROOT / "mssql-odbc" / "tests" / "e2e" / "run_e2e.ps1"


def test_windows_pr_e2e_bounds_build_and_test_workers():
    branch = next(
        value for step in load_template("build-template.yml")["steps"]
        for key, value in step.items()
        if key.startswith("${{ if ") and "parameters.enableOdbcE2E" in key
    )
    command = next(step["pwsh"] for step in branch if "pwsh" in step)
    assert "$buildJobs = [Environment]::ProcessorCount" in command
    assert "$testJobs = [Math]::Min(6, [Environment]::ProcessorCount)" in command
    assert "-BuildJobs $buildJobs -TestJobs $testJobs" in command
    assert "-Retries 3 -Coverage" in command
    assert "-CompareWithMsodbcsql" in command


def test_windows_arm_e2e_bounds_build_and_test_workers():
    steps = load_template("odbc-e2e-windows-remote-sql-template.yml")["steps"]
    command = next(s["pwsh"] for s in steps if s.get("displayName") == "Run ODBC C++ e2e tests")
    assert "$buildJobs = [Environment]::ProcessorCount" in command
    assert "$testJobs = [Math]::Min(4, [Environment]::ProcessorCount)" in command
    assert "-BuildJobs $buildJobs -TestJobs $testJobs" in command


def test_unix_pr_e2e_bounds_test_workers():
    container = (_TEMPLATES / "build-template-container.yml").read_text(encoding="utf-8")
    assert '-e ODBC_E2E_TEST_JOBS="$(( $(nproc) < 4 ? $(nproc) : 4 ))"' in container
    wrapper = (_ROOT / ".pipeline" / "scripts" / "containerized-odbc-e2e.sh").read_text(
        encoding="utf-8"
    )
    assert '--test-jobs="${ODBC_E2E_TEST_JOBS:-1}"' in wrapper
    stages = (_TEMPLATES / "validation-stages.yml").read_text(encoding="utf-8")
    assert '--test-jobs="$(( cpus < 4 ? cpus : 4 ))"' in stages


def test_rust_coverage_publishes_only_cobertura():
    template = (_TEMPLATES / "build-template.yml").read_text(encoding="utf-8")
    assert "--lcov" not in template
    assert "--cobertura" in template


def test_kerberos_archive_flag_matches_bash_comparison():
    template = (_TEMPLATES / "kerberos-test-template.yml").read_text(encoding="utf-8")
    # Template expressions render booleans as True/False.
    assert "USE_ARCHIVE: '${{ lower(parameters.useArchive) }}'" in template
    assert 'if [ "$USE_ARCHIVE" = "true" ]; then' in template


@pytest.mark.skipif(_BASH is None, reason="Bash is required")
@pytest.mark.parametrize(
    ("args", "expected"),
    [("", "1"), ("--test-jobs=4", "4"), ("--test-jobs=64", "64"),
     ("--test-jobs=0", None), ("--test-jobs=65", None), ("--test-jobs=x", None),
     ("--test-jobs=", None)],
)
def test_unix_e2e_validates_test_jobs(tmp_path, args, expected):
    runner = (_ROOT / "mssql-odbc" / "tests" / "e2e" / "run_e2e.sh").read_text(
        encoding="utf-8"
    )
    function = re.search(r"^parse_args\(\) \{\n.*?^\}", runner, re.M | re.S)
    assert function, "parse_args not found"
    script = f"""
set -euo pipefail
TEST_JOBS=1
CTEST_ARGS=()
usage() {{ :; }}
{function.group()}
parse_args {args}
printf "jobs:%s\\n" "$TEST_JOBS"
"""
    script_path = tmp_path / "parse.sh"
    script_path.write_text(script, encoding="utf-8", newline="\n")
    result = subprocess.run(
        [_BASH, script_path.name], cwd=tmp_path, capture_output=True, text=True,
    )
    if expected is None:
        assert result.returncode == 2
        assert "--test-jobs must be an integer from 1 to 64" in result.stderr
    else:
        assert result.returncode == 0, result.stdout + result.stderr
        assert f"jobs:{expected}" in result.stdout.splitlines()


@pytest.mark.skipif(
    any(shutil.which(tool) is None for tool in ("cmake", "ctest")),
    reason="needs CMake and CTest",
)
def test_e2e_ctest_registration_protects_shared_resources(tmp_path):
    # Configure the real project without downloading or compiling Google Test.
    gtest = tmp_path / "googletest"
    gtest.mkdir()
    (gtest / "CMakeLists.txt").write_text(
        "add_library(gtest INTERFACE)\nadd_library(gtest_main INTERFACE)\n",
        encoding="utf-8",
    )
    build = tmp_path / "build"
    result = subprocess.run(
        ["cmake", "-S", str(_WINDOWS_E2E.parent), "-B", str(build),
         f"-DFETCHCONTENT_SOURCE_DIR_GOOGLETEST={gtest}",
         "-DODBC_E2E_CONCURRENT=ON"],
        capture_output=True, text=True, timeout=120,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    result = subprocess.run(
        ["ctest", "--show-only=json-v1", "-C", "Debug"], cwd=build,
        capture_output=True, text=True, check=True,
    )
    tests = {
        test["name"]: {prop["name"]: prop["value"] for prop in test["properties"]}
        for test in json.loads(result.stdout)["tests"]
    }
    assert "escape_sequence_test_mt" in tests
    for name, props in tests.items():
        base = name.removesuffix("_mt")
        assert props["RESOURCE_LOCK"] == [base]
        assert props.get("RUN_SERIAL", False) == (base == "col_attribute_test")


@pytest.mark.skipif(
    any(shutil.which(tool) is None for tool in ("pwsh", "ctest")),
    reason="needs PowerShell and CTest",
)
@pytest.mark.parametrize("test_jobs", [1, 2, 4])
def test_windows_ctest_workers_preserve_isolation_retries_and_results(tmp_path, test_jobs):
    build = tmp_path / "build"
    build.mkdir()
    worker = tmp_path / "worker.py"
    worker.write_text(
        "import json, os, sys, time\n"
        "from pathlib import Path\n"
        "name = sys.argv[1]\n"
        "target = os.environ['ODBC_TEST_TARGET']\n"
        "path = Path(f'{target}-{name}.json')\n"
        "failed = Path(f'{target}-{name}-failed.json')\n"
        "retry = name == 'retry' and not failed.exists()\n"
        "if retry: path = failed\n"
        "start = time.monotonic()\n"
        "time.sleep(0.3)\n"
        "path.write_text(json.dumps(dict(start=start, end=time.monotonic(),\n"
        "    target=target, driver=os.environ['ODBC_TEST_DRIVER'],\n"
        "    dll=os.environ.get('MSSQL_ODBC_DLL', ''),\n"
        "    threads=os.environ['MSSQL_ODBC_MT_THREADS'])))\n"
        "sys.exit(1 if retry else 0)\n",
        encoding="utf-8",
    )
    names = ("one", "one_mt", "two", "exclusive", "retry")
    (build / "CTestTestfile.cmake").write_text(
        "\n".join(
            f'add_test({name} "{Path(sys.executable).as_posix()}" '
            f'"{worker.as_posix()}" {name})'
            for name in names
        ) + '\nset_tests_properties(one one_mt PROPERTIES RESOURCE_LOCK one)\n'
        'set_tests_properties(exclusive PROPERTIES RUN_SERIAL TRUE)\n',
        encoding="utf-8",
    )
    command = r"""
$ErrorActionPreference = 'Stop'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($env:RUNNER, [ref]$null, [ref]$null)
foreach ($name in @('Invoke-CtestRun', 'Assert-TestsExecuted')) {
    $function = $ast.Find({ param($n)
        $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        $n.Name -eq $name
    }, $true)
    . ([scriptblock]::Create($function.Extent.Text))
}
$ScriptDir = $env:FIXTURE
$TestJobs = [int]$env:TEST_JOBS
$Retries = 1
$FilterTests = ''
$ExcludeTests = ''
$script:MtStress = $false
$env:ODBC_TEST_TARGET = 'outer'
$env:ODBC_TEST_DRIVER = 'outer'
$env:MSSQL_ODBC_DLL = 'outer'
$env:MSSQL_ODBC_MT_THREADS = '8'
foreach ($leg in @('mssql-odbc', 'msodbcsql')) {
    $dll = if ($leg -eq 'mssql-odbc') { 'fixture.dll' } else { '' }
    $rc = Invoke-CtestRun $leg "junit-$leg.xml" "$leg driver" $dll @{ MSSQL_ODBC_MT_THREADS = '2' }
    if ($rc -ne 0) { throw "CTest failed: $rc" }
    Assert-TestsExecuted "$ScriptDir\build\junit-$leg.xml" $leg
    if ($env:ODBC_TEST_TARGET -ne 'outer' -or $env:ODBC_TEST_DRIVER -ne 'outer' -or
        $env:MSSQL_ODBC_DLL -ne 'outer' -or $env:MSSQL_ODBC_MT_THREADS -ne '8') {
        throw 'Leg environment was not restored'
    }
}
"""
    env = {**os.environ, "RUNNER": str(_WINDOWS_E2E), "FIXTURE": str(tmp_path),
           "TEST_JOBS": str(test_jobs), "CTEST_PARALLEL_LEVEL": "64"}
    result = subprocess.run(
        ["pwsh", "-NoProfile", "-Command", command], env=env,
        capture_output=True, text=True, timeout=30,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    legs = {}
    for leg in ("mssql-odbc", "msodbcsql"):
        assert f"{leg} leg executed 5 test(s)" in result.stdout
        assert (build / f"junit-{leg}.xml").is_file()
        records = {
            name: json.loads((build / f"{leg}-{name}.json").read_text(encoding="utf-8"))
            for name in names
        }
        records["retry-failed"] = json.loads(
            (build / f"{leg}-retry-failed.json").read_text(encoding="utf-8")
        )
        legs[leg] = records
        for record in records.values():
            assert record["driver"] == f"{leg} driver"
            assert record["dll"] == ("fixture.dll" if leg == "mssql-odbc" else "")
            assert record["threads"] == "2"

        def overlap(a, b):
            return a["start"] < b["end"] and b["start"] < a["end"]

        assert not overlap(records["one"], records["one_mt"])
        assert not any(
            overlap(records["exclusive"], record)
            for name, record in records.items() if name != "exclusive"
        )
        events = sorted(
            (record[key], delta) for record in records.values()
            for key, delta in (("start", 1), ("end", -1))
        )
        active = peak = 0
        for _, delta in events:
            active += delta
            peak = max(peak, active)
        assert peak <= test_jobs
        if test_jobs > 1:
            assert peak > 1
    assert max(record["end"] for record in legs["mssql-odbc"].values()) <= min(
        record["start"] for record in legs["msodbcsql"].values()
    )


def _require_windows_cpp_toolchain():
    vswhere = Path(os.environ["ProgramFiles(x86)"]) / (
        "Microsoft Visual Studio/Installer/vswhere.exe"
    )
    if not vswhere.is_file():
        pytest.skip("needs Visual Studio C++ build tools")
    compiler = subprocess.run(
        [str(vswhere), "-latest", "-products", "*", "-requires",
         "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"],
        capture_output=True, text=True, check=True,
    )
    if not compiler.stdout.strip():
        pytest.skip("needs Visual Studio C++ build tools")


@pytest.fixture(scope="module")
def windows_native_smoke(tmp_path_factory):
    _require_windows_cpp_toolchain()
    source = tmp_path_factory.mktemp("native-smoke")
    (source / "CMakeLists.txt").write_text(
        "cmake_minimum_required(VERSION 3.15)\nproject(fixture LANGUAGES CXX)\n"
        "add_executable(smoke_test smoke.cpp)\n",
        encoding="utf-8",
    )
    (source / "smoke.cpp").write_text(
        "#include <cstdlib>\n"
        "int main(int argc, char** argv) { return argc > 1 ? std::atoi(argv[1]) : 0; }\n",
        encoding="utf-8",
    )
    env = dict(os.environ)
    env.pop("CMAKE_GENERATOR", None)
    env.pop("CMAKE_GENERATOR_PLATFORM", None)
    build = source / "build"
    for args in (
        ["cmake", "-S", str(source), "-B", str(build)],
        ["cmake", "--build", str(build), "--config", "Debug", "--parallel", "2"],
    ):
        result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=120)
        assert result.returncode == 0, result.stdout + result.stderr
    return build / "Debug" / "smoke_test.exe"


@pytest.mark.skipif(
    sys.platform != "win32" or any(shutil.which(tool) is None for tool in ("pwsh", "cmake", "ctest")),
    reason="needs Windows CMake, CTest, and PowerShell",
)
@pytest.mark.parametrize("build_jobs", [None, 2])
def test_windows_build_only_parallelizes_and_stages_real_cpp_binaries(tmp_path, build_jobs):
    _require_windows_cpp_toolchain()
    producer = tmp_path / "producer" / "mssql-odbc" / "tests" / "e2e"
    producer.mkdir(parents=True)
    (producer / "CMakeLists.txt").write_text(
        "cmake_minimum_required(VERSION 3.15)\nproject(fixture LANGUAGES CXX)\n"
        "enable_testing()\nadd_executable(smoke_test smoke.cpp)\n"
        "add_test(NAME smoke_test COMMAND smoke_test)\n",
        encoding="utf-8",
    )
    (producer / "smoke.cpp").write_text("int main() { return 0; }\n", encoding="utf-8")
    finalize = producer.parents[1] / "scripts"
    finalize.mkdir()
    shutil.copy2(_ROOT / "mssql-odbc" / "scripts" / "finalize-artifact.ps1", finalize)
    target = tmp_path / "cargo-target" / "debug"
    target.mkdir(parents=True)
    for name in ("mssqlodbc.dll", "mssqlodbc.pdb"):
        (target / name).write_bytes(b"fixture")
    artifact = tmp_path / "drop"
    command = r"""
$ErrorActionPreference = 'Stop'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($env:RUNNER, [ref]$null, [ref]$null)
$main = $ast.EndBlock.Statements |
    Where-Object { $_ -is [System.Management.Automation.Language.TryStatementAst] } |
    Select-Object -Last 1
$mocks = @'
function cargo {
    if ($args[0] -eq 'metadata') {
        @{ target_directory = $env:CARGO_FIXTURE } | ConvertTo-Json -Compress
    } elseif ($args[0] -ne 'build') { throw "Unexpected cargo args: $args" }
    $global:LASTEXITCODE = 0
}
function cmake {
    ($args -join ' ') | Add-Content $env:CMAKE_LOG
    & $env:REAL_CMAKE @args
}
function Set-MtProfile { throw 'Build-only must not set a stress profile' }
function Initialize-DevSqlEnv { throw 'Build-only must not connect to SQL Server' }
function Save-OriginalRegistration { throw 'Build-only must not access the registry' }
function Restore-Registration { throw 'Build-only must not access the registry' }
'@
$source = [IO.File]::ReadAllText($env:RUNNER).Insert($main.Extent.StartOffset, $mocks + "`n")
$runner = Join-Path $env:PRODUCER 'run_e2e.ps1'
[IO.File]::WriteAllText($runner, $source)
$buildArgs = @{}
if ($env:BUILD_JOBS) { $buildArgs.BuildJobs = [int]$env:BUILD_JOBS }
Set-Location (Split-Path -Parent $env:DROP)
& $runner -BuildOnly -OutDir (Split-Path -Leaf $env:DROP) @buildArgs
"""
    log = tmp_path / "cmake.log"
    env = {**os.environ, "RUNNER": str(_WINDOWS_E2E), "PRODUCER": str(producer),
           "CARGO_FIXTURE": str(target.parent), "DROP": str(artifact),
           "REAL_CMAKE": shutil.which("cmake"), "CMAKE_LOG": str(log),
           "BUILD_JOBS": str(build_jobs) if build_jobs else ""}
    env.pop("CMAKE_GENERATOR", None)
    env.pop("CMAKE_GENERATOR_PLATFORM", None)
    result = subprocess.run(["pwsh", "-NoProfile", "-Command", command],
                            env=env, capture_output=True, text=True, timeout=120)
    assert result.returncode == 0, result.stdout + result.stderr
    expected_jobs = build_jobs if build_jobs else 1
    assert f"--build build --config Debug --parallel {expected_jobs}" in log.read_text(encoding="utf-8")
    assert (artifact / "build" / "Debug" / "smoke_test.pdb").is_file()
    assert not (artifact / "build" / "CMakeCache.txt").exists()
    shutil.rmtree(producer.parents[2])
    consumer = tmp_path / "relocated real build"
    shutil.copytree(artifact, consumer)
    result = subprocess.run(["ctest", "-C", "Debug", "--output-on-failure"],
                            cwd=consumer / "build", capture_output=True, text=True)
    assert result.returncode == 0, result.stdout + result.stderr
    assert "100% tests passed" in result.stdout


@pytest.mark.skipif(
    sys.platform != "win32" or any(shutil.which(tool) is None for tool in ("pwsh", "cmake", "ctest")),
    reason="needs Windows CMake, CTest, and PowerShell",
)
@pytest.mark.parametrize(("exit_code", "filter_tests"), [(0, ""), (1, ""), (0, "^missing$")])
def test_windows_prebuilt_drop_relocates_and_runs_both_legs_without_compiling(
    tmp_path, windows_native_smoke, exit_code, filter_tests
):
    producer = tmp_path / "producer checkout" / "tests" / "e2e"
    debug = producer / "build" / "Debug"
    debug.mkdir(parents=True)
    shutil.copy2(windows_native_smoke, debug / "smoke_test.exe")
    for name in ("smoke_test.pdb", "mssqlodbc.dll", "mssqlodbc.pdb", "unused.lib"):
        (debug / name).write_bytes(b"fixture")
    (producer / "build" / "CMakeCache.txt").write_text("do not ship", encoding="utf-8")
    exe = (debug / "smoke_test.exe").as_posix()
    (producer / "build" / "CTestTestfile.cmake").write_text(
        f'add_test(smoke_test "{exe}" {exit_code})\n'
        f'add_test(smoke_test_mt "{exe}" {exit_code})\n'
        'set_tests_properties(smoke_test_mt PROPERTIES LABELS mt TIMEOUT 30 '
        'SKIP_RETURN_CODE 77)\nsubdirs("nested")\n',
        encoding="utf-8",
    )
    nested = producer / "build" / "nested"
    nested.mkdir()
    (nested / "CTestTestfile.cmake").write_text(
        f'add_test(nested_test "{exe}" 0)\n', encoding="utf-8"
    )
    artifact = tmp_path / "published drop"
    # Execute the production packaging function, without building or touching HKLM.
    command = r"""
$ErrorActionPreference = 'Stop'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($env:RUNNER, [ref]$null, [ref]$null)
$function = $ast.Find({ param($n)
    $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
    $n.Name -eq 'Export-PrebuiltTests'
}, $true)
. ([scriptblock]::Create($function.Extent.Text))
$ScriptDir = $env:PRODUCER
Export-PrebuiltTests "$ScriptDir\build\Debug\mssqlodbc.dll" $env:DROP
"""
    env = {**os.environ, "RUNNER": str(_WINDOWS_E2E), "PRODUCER": str(producer),
           "DROP": str(artifact)}
    result = subprocess.run(["pwsh", "-NoProfile", "-Command", command],
                            env=env, capture_output=True, text=True)
    assert result.returncode == 0, result.stdout + result.stderr
    assert not (artifact / "build" / "CMakeCache.txt").exists()
    assert not (artifact / "build" / "Debug" / "unused.lib").exists()
    assert (artifact / "build" / "Debug" / "mssqlodbc.pdb").is_file()
    shutil.rmtree(producer.parents[1])
    consumer = tmp_path / "different consumer path" / "tests" / "e2e"
    shutil.copytree(artifact, consumer)
    registration = subprocess.run(
        ["ctest", "--show-only=json-v1", "-C", "Debug"], cwd=consumer / "build",
        capture_output=True, text=True, check=True,
    )
    tests = json.loads(registration.stdout)["tests"]
    assert {t["name"] for t in tests} == {"smoke_test", "smoke_test_mt", "nested_test"}
    for test in tests:
        assert Path(test["command"][0]).resolve() == consumer / "build" / "Debug" / "smoke_test.exe"
    props = {p["name"]: p["value"] for p in next(
        t for t in tests if t["name"] == "smoke_test_mt"
    )["properties"]}
    assert props["LABELS"] == ["mt"]
    assert props["TIMEOUT"] == 30
    assert props["SKIP_RETURN_CODE"] == 77

    # Isolate registry access; execute the production main block, CTest, stress
    # selection and parity reporting. Any compile attempt is a test failure.
    command = r"""
$ErrorActionPreference = 'Stop'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($env:RUNNER, [ref]$null, [ref]$null)
$main = $ast.EndBlock.Statements |
    Where-Object { $_ -is [System.Management.Automation.Language.TryStatementAst] } |
    Select-Object -Last 1
$mocks = @'
function cargo { throw 'Unexpected Cargo invocation in prebuilt mode' }
function cmake { throw 'Unexpected CMake invocation in prebuilt mode' }
function Save-OriginalRegistration {}
function Register-RustDriver([string]$DriverPath) {
    if (-not (Test-Path $DriverPath)) { throw 'Missing driver' }
}
function Restore-Registration {}
function Get-MsodbcsqlDriverPath { return "$PSScriptRoot\reference.dll" }
'@
$source = [IO.File]::ReadAllText($env:RUNNER).Insert($main.Extent.StartOffset, $mocks + "`n")
$runner = Join-Path $env:CONSUMER 'run_e2e.ps1'
[IO.File]::WriteAllText($runner, $source)
& $runner -SkipBuild -MtProfile nightly -CompareWithMsodbcsql -TestJobs 2 -FilterTests $env:FIXTURE_FILTER
"""
    (consumer / "reference.dll").write_bytes(b"reference")
    dumps = consumer / "build" / "mt-dumps"
    dumps.mkdir()
    (dumps / "fixture.dmp").write_bytes(b"fixture")
    env.update(CONSUMER=str(consumer), ODBC_TEST_SERVER="fixture",
               MSSQL_ODBC_MT_THREADS="8", MSSQL_ODBC_MT_ITERS="20",
               MSSQL_ODBC_MT_SKIP_MODES="none", FIXTURE_FILTER=filter_tests)
    result = subprocess.run(["pwsh", "-NoProfile", "-Command", command],
                            env=env, capture_output=True, text=True)
    assert {p.name for p in (dumps / "symbols").iterdir()} == {
        "mssqlodbc.dll", "mssqlodbc.pdb", "smoke_test.exe", "smoke_test.pdb"
    }
    if exit_code or filter_tests:
        assert result.returncode != 0
        expected = "Parity check FAILED" if exit_code else "No tests were executed"
        assert expected in result.stdout + result.stderr
        return
    assert result.returncode == 0, result.stdout + result.stderr
    assert "MT profile: nightly (threads=8 iterations=20" in result.stdout
    for leg in ("mssql-odbc", "msodbcsql"):
        junit = (consumer / "build" / f"junit-{leg}.xml").read_text(encoding="utf-8")
        assert 'name="smoke_test_mt"' in junit
        assert 'name="smoke_test"' not in junit
        assert 'name="nested_test"' not in junit


@pytest.mark.skipif(shutil.which("pwsh") is None, reason="needs pwsh")
@pytest.mark.parametrize(
    ("args", "error"),
    [
        (["-BuildOnly"], "-BuildOnly and -OutDir"),
        (["-OutDir", "unused"], "-BuildOnly and -OutDir"),
        (["-BuildOnly", "-SkipBuild"], "-BuildOnly cannot be combined"),
        (["-BuildOnly", "-Release", "-OutDir", "unused"],
         "-BuildOnly cannot be combined with -Release"),
        (["-SkipBuild", "-Coverage"], "-SkipBuild cannot be combined"),
        (["-SkipBuild", "-Release"], "-SkipBuild cannot be combined with -Release"),
        (["-BuildJobs", "0"], "BuildJobs"),
        (["-TestJobs", "0"], "TestJobs"),
        (["-TestJobs", "65"], "TestJobs"),
    ],
)
def test_windows_e2e_rejects_incompatible_build_options(args, error):
    result = subprocess.run(
        ["pwsh", "-NoProfile", "-File", str(_WINDOWS_E2E), *args],
        capture_output=True, text=True,
    )
    assert result.returncode != 0
    assert error in result.stderr


@pytest.mark.skipif(
    sys.platform != "win32" or shutil.which("powershell.exe") is None,
    reason="needs Windows PowerShell",
)
def test_windows_build_only_rejects_legacy_powershell_before_building(tmp_path):
    script = str(_WINDOWS_E2E).replace("'", "''")
    drop = str(tmp_path / "drop").replace("'", "''")
    command = (
        "function cargo { throw 'Unexpected build' }; "
        f"& '{script}' -BuildOnly -OutDir '{drop}'"
    )
    result = subprocess.run(
        ["powershell.exe", "-NoProfile", "-Command", command],
        capture_output=True, text=True, timeout=30,
    )
    assert result.returncode != 0
    assert "-BuildOnly requires PowerShell 7 or later" in result.stderr
    assert "Unexpected build" not in result.stderr
    assert not (tmp_path / "drop").exists()


def test_windows_stress_groups_cover_current_concurrent_suites(windows_stress_job):
    e2e = _ROOT / "mssql-odbc" / "tests" / "e2e"
    concurrent = {
        f"{source.stem}_mt"
        for source in (e2e / "tests").rglob("*.cpp")
        if re.search(r"^ODBC_TEST_F(_MODES)?\(", source.read_text(encoding="utf-8"), re.M)
    }
    assert concurrent
    for name, group in windows_stress_job["strategy"]["matrix"].items():
        selected = {
            suite for suite in concurrent
            if (not group["filterTests"] or re.search(group["filterTests"], suite))
            and (not group["excludeTests"] or not re.search(group["excludeTests"], suite))
        }
        assert selected, f"{name} selects no concurrent suites"


@pytest.mark.skipif(shutil.which("ctest") is None, reason="CTest is required")
def test_windows_stress_ctest_groups_are_exhaustive_and_disjoint(tmp_path, windows_stress_job):
    cmake = (_ROOT / "mssql-odbc" / "tests" / "e2e" / "CMakeLists.txt").read_text(
        encoding="utf-8"
    )
    # Model every suite as converted, including ones not converted yet.
    suites = set(re.findall(r"^add_odbc_(?:direct_load_)?test\((\w+)", cmake, re.M))
    suites.add("future_suite_test")
    entries = {f"{suite}_mt" for suite in suites}
    testfile = tmp_path / "CTestTestfile.cmake"
    testfile.write_text(
        "\n".join(
            f'add_test("{suite}" "unused")\n'
            f'add_test("{suite}_mt" "unused")\n'
            f'set_tests_properties("{suite}_mt" PROPERTIES LABELS "mt")'
            for suite in sorted(suites)
        ),
        encoding="utf-8",
    )
    seen = set()
    for name, group in windows_stress_job["strategy"]["matrix"].items():
        args = [shutil.which("ctest"), "--show-only=json-v1", "-L", "mt"]
        if group["filterTests"]:
            args += ["-R", group["filterTests"]]
        if group["excludeTests"]:
            args += ["-E", group["excludeTests"]]
        result = subprocess.run(args, cwd=tmp_path, capture_output=True, text=True, timeout=30)
        assert result.returncode == 0, result.stdout + result.stderr
        selected = {test["name"] for test in json.loads(result.stdout)["tests"]}
        assert selected, f"{name} selects no suites"
        assert not seen & selected, f"{name} duplicates {seen & selected}"
        seen.update(selected)
    assert seen == entries


def test_windows_stress_results_and_artifacts_are_group_scoped(windows_stress_job):
    def artifacts(steps):
        for step in steps:
            if step.get("task") == "PublishPipelineArtifact@1":
                yield step["inputs"]["artifact"]
            for key, value in step.items():
                if key.startswith("${{ if "):
                    yield from artifacts(value)

    names = list(artifacts(windows_stress_job["steps"]))
    assert len(names) == 3
    assert len(set(names)) == len(names)
    for name in names:
        assert "$(suiteGroup)" in name
        assert "$(System.JobAttempt)" in name
    results = [
        step for step in windows_stress_job["steps"]
        if step.get("task") == "PublishTestResults@2"
    ]
    assert len(results) == 2
    for step in results:
        assert "$(suiteGroup)" in step["inputs"]["testRunTitle"]
        assert step["condition"] == "succeededOrFailed()"


@pytest.mark.skipif(_BASH is None, reason="Bash is required")
@pytest.mark.parametrize(
    ("profile", "threads", "iterations"),
    [("pr", "2", "1"), ("nightly", "8", "20"), ("weekly", "16", "50")],
)
@pytest.mark.parametrize(
    ("override", "expected"),
    [
        ("unset MSSQL_ODBC_MT_SKIP_MODES", "SharedStmt"),
        ("export MSSQL_ODBC_MT_SKIP_MODES=''", "SharedStmt"),
        ("export MSSQL_ODBC_MT_SKIP_MODES=none", "none"),
        ("export MSSQL_ODBC_MT_SKIP_MODES=SharedEnv,SharedDbc", "SharedEnv,SharedDbc"),
    ],
)
def test_unix_odbc_profiles_skip_shared_stmt(tmp_path, profile, threads, iterations,
                                           override, expected):
    runner = (_ROOT / "mssql-odbc" / "tests" / "e2e" / "run_e2e.sh").read_text(
        encoding="utf-8"
    )
    function = re.search(r"^apply_mt_profile\(\) \{\n.*?^\}", runner, re.M | re.S)
    assert function, "apply_mt_profile not found"
    script = f"""
set -euo pipefail
unset MSSQL_ODBC_MT_THREADS MSSQL_ODBC_MT_ITERS MSSQL_ODBC_MT_GDB ODBC_E2E_CONCURRENT
{override}
MT_PROFILE={profile}
RETRIES=0
CTEST_ARGS=()
MS_LEG_ENV=()
{function.group()}
apply_mt_profile
bash -c 'printf "rust:%s:%s:%s\\n" "$MSSQL_ODBC_MT_SKIP_MODES" \
"${{MSSQL_ODBC_MT_THREADS:-2}}" "${{MSSQL_ODBC_MT_ITERS:-1}}"'
env ${{MS_LEG_ENV[@]+"${{MS_LEG_ENV[@]}}"}} \
bash -c 'printf "reference:%s:%s:%s\\n" "$MSSQL_ODBC_MT_SKIP_MODES" \
"${{MSSQL_ODBC_MT_THREADS:-2}}" "${{MSSQL_ODBC_MT_ITERS:-1}}"'
printf "concurrent:%s\\n" "${{ODBC_E2E_CONCURRENT:-}}"
"""
    script_path = tmp_path / "profile.sh"
    script_path.write_text(script, encoding="utf-8", newline="\n")
    result = subprocess.run(
        [_BASH, script_path.name], cwd=tmp_path, capture_output=True, text=True,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert f"rust:{expected}:{threads}:{iterations}" in result.stdout.splitlines()
    assert f"reference:{expected}:2:1" in result.stdout.splitlines()
    # Unix builds compile the <name>_mt entries only for the stress profiles.
    concurrent = "" if profile == "pr" else "ON"
    assert f"concurrent:{concurrent}" in result.stdout.splitlines()


@pytest.mark.parametrize(
    "selector",
    [
        "scripts/test_bump_released_versions.py",
        "scripts/test_bump_mssql_python_release.py",
        "scripts/test_validation_pipeline.py",
    ],
)
def test_pipeline_tests_run_in_shared_validation(selector):
    stages = load_template("validation-stages.yml")["stages"]
    build = next(stage for stage in stages if stage["stage"] == "Build")
    windows = next(job for job in build["jobs"] if job.get("job") == "Build_Windows")
    step = next(
        step for step in windows["steps"]
        if step.get("displayName") == "Unit test packaging scripts and pipeline configuration"
    )
    command = next(line for line in step["pwsh"].splitlines() if "python -m pytest" in line)
    assert selector in command.split()
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)


def test_release_automation_bash_oracles_run_on_linux():
    stages = load_template("validation-stages.yml")["stages"]
    build = next(stage for stage in stages if stage["stage"] == "Build")
    linux = next(job for job in build["jobs"] if job.get("job") == "Build_Linux")
    step = next(
        step
        for step in linux["steps"]
        if step.get("displayName") == "Test release automation Bash oracles"
    )
    command = step["bash"]
    assert "test_required_bash_oracles_have_usable_bash" in command
    assert "test_pipeline_token_extraction_finds_the_repo_scoped_key" in command
    assert "test_cadence_guard_selects_the_first_and_third_tuesday" in command
    assert "test_worker_token_extraction_finds_the_repo_scoped_key" in command
    assert step["env"]["REQUIRE_BASH_ORACLES"] == "1"
    assert not step.get("continueOnError", False)


@pytest.mark.parametrize(
    ("service", "ports"),
    [
        ("dc", ["8053:53/tcp", "8053:53/udp", "8888:88/tcp", "8888:88/udp",
                "8389:389", "8636:636"]),
        ("mssql", ["1433:1433"]),
    ],
)
def test_kerberos_matrix_preserves_ports_on_loopback(service, ports):
    compose = yaml.safe_load(
        (_ROOT / "kerberos-test" / "docker-compose-matrix.yml").read_text(encoding="utf-8")
    )
    assert compose["services"][service]["ports"] == [
        f"127.0.0.1:{port}" for port in ports
    ]


def test_kerberos_matrix_only_publishes_infrastructure_on_loopback():
    compose = yaml.safe_load(
        (_ROOT / "kerberos-test" / "docker-compose-matrix.yml").read_text(encoding="utf-8")
    )
    published = {
        name: service["ports"]
        for name, service in compose["services"].items() if service.get("ports")
    }
    assert set(published) == {"dc", "mssql"}
    for service, ports in published.items():
        for port in ports:
            assert isinstance(port, str) and port.startswith("127.0.0.1:"), (service, port)


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


def test_macos_docker_condition_preserves_other_callers():
    template = load_template("macos-docker-steps.yml")
    condition = next(p for p in template["parameters"] if p["name"] == "condition")
    assert condition == {"name": "condition", "type": "string", "default": "succeeded()"}
    for step in template["steps"]:
        assert step["condition"] == "${{ parameters.condition }}"
        assert not step.get("continueOnError", False)

    sql_steps = load_template("sql-setup-template.yml")["steps"]
    macos = next(
        step["${{ if eq(parameters.buildTarget, 'MacOS') }}"]
        for step in sql_steps if "${{ if eq(parameters.buildTarget, 'MacOS') }}" in step
    )
    docker = next(step for step in macos if step.get("template") == "macos-docker-steps.yml")
    assert "condition" not in docker.get("parameters", {})
    sql = next(step for step in macos if "start-sql-server-macos.sh" in step.get("script", ""))
    assert sql.get("condition", "succeeded()") == "succeeded()"


def test_mssql_python_macos_readiness_gates_provisioning():
    steps = load_template("test-mssql-python-macos-template.yml")["steps"]
    named = {step["displayName"]: step for step in steps if "displayName" in step}
    ready = "and(succeeded(), eq(variables['mssqlPythonReady'], 'true'))"
    docker = next(step for step in steps if step.get("template") == "macos-docker-steps.yml")
    assert docker["parameters"]["condition"] == ready
    sql = named["Start SQL Server (Docker, no TLS certs)"]
    tests = named["Run mssql-python tests"]
    assert sql["condition"] == tests["condition"] == ready
    smoke = named["Verify installed mssql-python runtime"]
    assert steps.index(smoke) < steps.index(docker) < steps.index(sql) < steps.index(tests)
    assert smoke.get("condition", "succeeded()") == "succeeded()"
    for step in steps:
        assert not step.get("continueOnError", False)

    setup_results = named["Publish mssql-python macOS setup results"]
    test_results = named["Publish mssql-python macOS test results"]
    assert setup_results["condition"] == "succeededOrFailed()"
    assert test_results["condition"] == (
        "and(succeededOrFailed(), eq(variables['mssqlPythonReady'], 'true'))"
    )
    for publish in (setup_results, test_results):
        assert publish["inputs"]["failTaskOnMissingResultsFile"] is True
    assert named["Cleanup SQL Server"]["condition"] == (
        "and(always(), eq(variables['mssqlPythonReady'], 'true'))"
    )
    assert named["Cleanup"]["condition"] == "always()"


@pytest.mark.skipif(_BASH is None, reason="Bash is required")
@pytest.mark.parametrize("count", [0, 1, 2])
def test_odbc_wheel_installation(tmp_path, count):
    steps = load_template("test-mssql-python-macos-template.yml")["steps"]
    script = next(step["script"] for step in steps if step.get("displayName") == "Install mssql-python packages")
    source = tmp_path / "rust"
    source.mkdir()
    wheels = tmp_path / "mssql-python" / "odbc-dist"
    wheels.mkdir(parents=True)
    for index in range(count):
        (wheels / f"mssql_python_odbc-{index}.whl").touch()
    result = subprocess.run(
        [_BASH, "-s"], cwd=source, capture_output=True, text=True,
        input='python() { printf "%s\\t" "$@"; printf "\\n"; }\n'
        + script.replace("$(Build.SourcesDirectory)", "$PWD"),
    )
    assert result.returncode == (0 if count == 1 else 1), result.stdout + result.stderr
    if count == 1:
        installs = [line.split("\t")[:-1] for line in result.stdout.splitlines()]
        assert len(installs) == 2
        assert installs[0][:4] == ["-m", "pip", "install", "--no-deps"]
        assert installs[0][4].endswith("mssql_python_odbc-0.whl")
        assert installs[1][:5] == ["-m", "pip", "install", "--no-deps", "-e"]
    else:
        assert f"Expected exactly one mssql-python-odbc wheel, found {count}" in result.stdout
        assert "\t" not in result.stdout


@pytest.fixture
def installed_runtime(tmp_path, monkeypatch):
    driver = tmp_path / "driver.dylib"
    driver.touch()
    provider = {"id": "msodbcsql18", "driver_path": str(driver)}
    module = SimpleNamespace(get_native_provider_info=lambda: provider)
    monkeypatch.setitem(sys.modules, "mssql_python", module)
    monkeypatch.setattr(verify_python.metadata, "requires", lambda _: [])
    return module, provider


@pytest.mark.parametrize(
    ("requirement", "installed", "error"),
    [
        ("MSSQL_PYTHON_RS==9.9", "0.2", None),
        ("mssql-python-odbc==9.9", "0.2", None),
        ("new-dependency>=2", "3", None),
        ("new-dependency>=2", None, "Missing runtime dependency"),
        ("new-dependency>=2", "1", "update setup requirements"),
        ("inactive; python_version < '2'", None, None),
        ("optional; extra == 'feature'", None, None),
        ("new-dependency[feature]", "1", "explicit setup support"),
        ("new-dependency @ https://example.invalid/package.whl", "1", "explicit setup support"),
        ("mssql-python-rs[feature]", "0.2", "explicit setup support"),
        ("mssql-python-odbc @ https://example.invalid/package.whl", "18", "explicit setup support"),
    ],
)
def test_runtime_requirements(installed_runtime, monkeypatch, requirement, installed, error):
    monkeypatch.setattr(verify_python.metadata, "requires", lambda _: [requirement])

    def version(_):
        if installed is None:
            raise verify_python.metadata.PackageNotFoundError(requirement)
        return installed

    monkeypatch.setattr(verify_python.metadata, "version", version)
    if error:
        with pytest.raises((AssertionError, pytest.fail.Exception), match=error):
            verify_python.test_installed_runtime()
    else:
        verify_python.test_installed_runtime()


@pytest.mark.parametrize("reason", [None, "PullRequest", "IndividualCI"])
@pytest.mark.parametrize(
    "case", ["ok", "bad-id", "missing-path", "nonexistent-path", "missing-api",
             "required-argument", "query-error", "import-error"],
)
def test_provider_failure_policy(installed_runtime, monkeypatch, capsys, reason, case):
    module, provider = installed_runtime
    if reason is None:
        monkeypatch.delenv("BUILD_REASON", raising=False)
    else:
        monkeypatch.setenv("BUILD_REASON", reason)
    if case == "bad-id":
        provider["id"] = "other"
    elif case == "missing-path":
        provider.pop("driver_path")
    elif case == "nonexistent-path":
        provider["driver_path"] += ".missing"
    elif case == "missing-api":
        del module.get_native_provider_info
    elif case == "required-argument":
        module.get_native_provider_info = lambda required: provider
    elif case == "query-error":
        def broken_query():
            raise TypeError("unexpected provider failure")
        module.get_native_provider_info = broken_query
    elif case == "import-error":
        monkeypatch.setitem(sys.modules, "mssql_python", None)
    if case == "ok":
        verify_python.test_installed_runtime()
    else:
        expected = {"query-error": TypeError, "import-error": ModuleNotFoundError}.get(
            case, pytest.skip.Exception if reason == "IndividualCI" else pytest.fail.Exception,
        )
        with pytest.raises(expected):
            verify_python.test_installed_runtime()
    output = capsys.readouterr().out
    assert ("mssqlPythonReady]true" in output) == (case == "ok")
    assert ("type=warning" in output) == (
        reason == "IndividualCI" and case not in ("ok", "query-error", "import-error")
    )


def test_pin_validation_is_not_path_filtered_or_optional():
    pipeline = yaml.safe_load(
        (_ROOT / ".pipeline" / "validation-pipeline.yml").read_text(encoding="utf-8")
    )
    # Server-side PR filters also need to include .pipeline/ (see scripts/README.md).
    assert "pr" not in pipeline
    stages = load_template("validation-stages.yml")["stages"]
    stage = next(stage for stage in stages if stage["stage"] == "Build_mssql_python")
    assert stage["dependsOn"] == ["InitializeValidation"]
    assert stage["condition"] == (
        "and(not(canceled()), "
        "eq(dependencies.InitializeValidation.result, 'Succeeded'), "
        "eq('${{ parameters.RunFuzz }}', 'false'), "
        "eq('${{ parameters.RunLongHaul }}', 'false'), "
        "or(ne(variables['Build.Reason'], 'PullRequest'), "
        "ne(dependencies.InitializeValidation.outputs['Initialize.SetDuplicateState.skipDuplicate'], 'true')))"
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
    ("env", "expected"),
    [("", None), ("PYTEST_MARK_EXPR='not slow'", "not slow"), ("PYTEST_MARK_EXPR=''", "")],
)
def test_odbc_runner_forwards_marker_expression(tmp_path, env, expected):
    (tmp_path / "tests").mkdir()
    (tmp_path / "tests" / "test_a.py").touch()
    runner = _ROOT / ".pipeline" / "scripts" / "run-mssql-python-odbc-tests.sh"
    (tmp_path / "runner.sh").write_text(runner.read_text(encoding="utf-8"), newline="\n")
    script = f"""
    python() {{ return 0; }}
    timeout() {{ printf '%s\\n' "$@" > "$PWD/args"; }}
    {env} MSSQL_PYTHON_DIR="$2" TEST_RESULTS_DIR="$2/test-results" \
        PYTEST_FILE_TIMEOUT=10s PYTEST_TOTAL_BUDGET=120s source "$1"
    """
    script_path = tmp_path / "run-test.sh"
    script_path.write_text(script, encoding="utf-8", newline="\n")
    result = subprocess.run(
        [_BASH, script_path.name, "./runner.sh", "."],
        cwd=tmp_path, capture_output=True, text=True,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    args = (tmp_path / "args").read_text(encoding="utf-8").splitlines()
    pytest_args = args[args.index("tests/test_a.py"):]
    if expected is None:
        assert "-m" not in pytest_args
    else:
        assert pytest_args[pytest_args.index("-m") + 1] == expected


@pytest.mark.skipif(_BASH is None, reason="Bash is required")
@pytest.mark.parametrize(
    ("env", "expected"),
    [
        ("", []),
        ("PYTEST_DESELECT=''", []),
        ("PYTEST_DESELECT='tests/test_a.py::test_x'", ["tests/test_a.py::test_x"]),
        (
            "PYTEST_DESELECT='tests/test_a.py::test_x  tests/test_b.py::test_y[1]'",
            ["tests/test_a.py::test_x", "tests/test_b.py::test_y[1]"],
        ),
        (
            "PYTEST_DESELECT=$'tests/test_a.py::test_x\\n\\ttests/test_b.py::test_y\\n'",
            ["tests/test_a.py::test_x", "tests/test_b.py::test_y"],
        ),
    ],
)
def test_odbc_runner_forwards_deselected_tests(tmp_path, env, expected):
    (tmp_path / "tests").mkdir()
    (tmp_path / "tests" / "test_a.py").touch()
    runner = _ROOT / ".pipeline" / "scripts" / "run-mssql-python-odbc-tests.sh"
    (tmp_path / "runner.sh").write_text(runner.read_text(encoding="utf-8"), newline="\n")
    script = f"""
    python() {{ return 0; }}
    timeout() {{ printf '%s\\n' "$@" > "$PWD/args"; }}
    {env} MSSQL_PYTHON_DIR="$2" TEST_RESULTS_DIR="$2/test-results" \
        PYTEST_FILE_TIMEOUT=10s PYTEST_TOTAL_BUDGET=120s source "$1"
    """
    script_path = tmp_path / "run-test.sh"
    script_path.write_text(script, encoding="utf-8", newline="\n")
    result = subprocess.run(
        [_BASH, script_path.name, "./runner.sh", "."],
        cwd=tmp_path, capture_output=True, text=True,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    args = (tmp_path / "args").read_text(encoding="utf-8").splitlines()
    pytest_args = args[args.index("tests/test_a.py"):]
    deselected = [pytest_args[i + 1] for i, arg in enumerate(pytest_args) if arg == "--deselect"]
    assert deselected == expected


def test_mssql_python_odbc_job_deselects_only_timing_flaky_tests():
    template = load_template("test-mssql-python-odbc-template.yml")
    params = {p["name"]: p["default"] for p in template["parameters"]}
    assert params["pytestDeselect"] == [
        "tests/test_021_concurrent_connection_perf.py::test_concurrent_connection_gil_release",
    ]
    step = next(
        step for step in template["steps"]
        if step.get("displayName") == "Run mssql-python tests against mssql-odbc"
    )
    assert "PYTEST_DESELECT=\"${{ join(' ', parameters.pytestDeselect) }}\"" in step["script"]


def test_mssql_python_odbc_job_includes_non_slow_stress_tests():
    template = load_template("test-mssql-python-odbc-template.yml")
    params = {p["name"]: p["default"] for p in template["parameters"]}
    assert params["pytestMarker"] == "not slow"
    step = next(
        step for step in template["steps"]
        if step.get("displayName") == "Run mssql-python tests against mssql-odbc"
    )
    assert 'PYTEST_MARK_EXPR="${{ parameters.pytestMarker }}"' in step["script"]


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
    guide = (_ROOT / "mssql-odbc" / "docs" / "testing.md").read_text(encoding="utf-8")
    assert set(re.findall(r"nightly-\d{4}-\d{2}-\d{2}", guide)) == {toolchain}, (
        "Update mssql-odbc/docs/testing.md's Miri version when changing miriToolchain"
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


def test_macos_builds_set_a_supported_deployment_target():
    steps = load_template("build-template.yml")["steps"]
    group = next(step for step in steps if "${{ if eq(parameters.osType, 'MacOS') }}" in step)
    setup = group["${{ if eq(parameters.osType, 'MacOS') }}"][0]
    assert setup["displayName"] == "Set macOS deployment target"
    assert "${MACOSX_DEPLOYMENT_TARGET:-10.15}" in setup["bash"]
    assert "task.setvariable variable=MACOSX_DEPLOYMENT_TARGET" in setup["bash"]
    odbc = load_template("build-odbc-macos-template.yml")["steps"]
    build = next(step for step in odbc if "cargo build" in step.get("script", ""))
    assert build["script"].index("export MACOSX_DEPLOYMENT_TARGET") < build["script"].index("cargo build")
    assert "${MACOSX_DEPLOYMENT_TARGET:-10.15}" in build["script"]
    sqlcmd = (_ROOT / ".pipeline" / "scripts" / "build-mssql-sqlcmd-native.sh").read_text()
    assert '*-apple-darwin*) export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-10.15}"' in sqlcmd
    python = load_template("test-mssql-python-macos-template.yml")["steps"]
    build = next(step for step in python if "maturin build" in step.get("script", ""))
    assert build["script"].index("export MACOSX_DEPLOYMENT_TARGET") < build["script"].index("maturin build")
    assert "${MACOSX_DEPLOYMENT_TARGET:-10.15}" in build["script"]


@pytest.mark.skipif(_BASH is None, reason="Bash is required")
@pytest.mark.parametrize(
    ("deployment_target", "expected"),
    [(None, "10.15"), ("", "10.15"), ("10.15", "10.15"), ("26.0", "26.0")],
)
def test_macos_python_build_preserves_supported_deployment_targets(deployment_target, expected):
    steps = load_template("test-mssql-python-macos-template.yml")["steps"]
    build = next(step for step in steps if "maturin build" in step.get("script", ""))
    export = next(
        line for line in build["script"].splitlines()
        if line.startswith("export MACOSX_DEPLOYMENT_TARGET=")
    )
    setup = (
        "unset MACOSX_DEPLOYMENT_TARGET"
        if deployment_target is None
        else f"export MACOSX_DEPLOYMENT_TARGET='{deployment_target}'"
    )
    result = subprocess.run(
        [_BASH, "-c", f'{setup}\n{export}\nprintf "%s" "$MACOSX_DEPLOYMENT_TARGET"'],
        cwd=_ROOT, capture_output=True, text=True,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout == expected

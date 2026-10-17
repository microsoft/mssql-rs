# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Shared SQL credential contracts; service-side masking still needs live ADO."""

import importlib.util
import os
from functools import lru_cache
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys
import time
import zipfile

import pytest
import yaml

ROOT = Path(__file__).parents[1]
TEMPLATES = ROOT / ".pipeline" / "templates"
SCRIPTS = ROOT / ".pipeline" / "scripts" / "sql-host"
BASH = shutil.which("bash")
PWSH = shutil.which("pwsh")
SPEC = importlib.util.spec_from_file_location("shared_password", SCRIPTS / "generate-shared-password.py")
generator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generator)
SUFFIX = "-$(SQL_CREDENTIAL_GENERATION)-$(System.StageAttempt)"


@lru_cache
def template(name):
    return yaml.safe_load((TEMPLATES / name).read_text(encoding="utf-8"))


def nodes(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from nodes(child)
    elif isinstance(value, list):
        for child in value:
            yield from nodes(child)


def credential():
    return "Aa1!" + secrets.token_hex(24)


def test_native_shell_is_available():
    if sys.platform == "win32":
        assert PWSH, "PowerShell is required for Windows credential regression tests"
    elif sys.platform.startswith("linux"):
        assert BASH, "Bash is required for Linux credential regression tests"


@pytest.mark.parametrize(("platform", "shell"), [("win32", "PWSH"), ("linux", "BASH")])
def test_native_shell_gate_rejects_missing_interpreter(monkeypatch, platform, shell):
    monkeypatch.setattr(sys, "platform", platform)
    monkeypatch.setitem(globals(), shell, None)
    with pytest.raises(AssertionError, match="is required"):
        test_native_shell_is_available()


def test_linux_execution_is_wired_before_build():
    build = next(s for s in template("validation-stages.yml")["stages"] if s.get("stage") == "Build")
    linux = next(job for job in build["jobs"] if job.get("job") == "Build_Linux")
    step = next(
        step for step in linux["steps"]
        if step.get("displayName") == "Test shared SQL credential scripts (Linux)"
    )
    assert "set -euo pipefail" in step["bash"]
    assert "-m pytest scripts/test_shared_sql_credentials.py -q" in step["bash"]
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)
    container = next(step for step in linux["steps"] if step.get("template") == "build-template-container.yml")
    assert linux["steps"].index(step) < linux["steps"].index(container)


def test_generator_policy_randomness_and_output_only(capsys, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    passwords, generations = set(), set()
    for _ in range(64):
        generator.main()
        lines = capsys.readouterr().out.splitlines()
        assert len(lines) == 2
        prefix = "##vso[task.setvariable variable=SQL_PASSWORD;isSecret=true;isOutput=true]"
        assert lines[0].startswith(prefix)
        password = lines[0][len(prefix):]
        assert re.fullmatch(r"Aa1![0-9a-f]{48}", password)
        generation_prefix = "##vso[task.setvariable variable=SQL_CREDENTIAL_GENERATION;isOutput=true]"
        assert lines[1].startswith(generation_prefix)
        generation = lines[1][len(generation_prefix):]
        assert re.fullmatch(r"[0-9a-f]{32}", generation)
        passwords.add(password)
        generations.add(generation)
    assert len(passwords) == len(generations) == 64
    assert list(tmp_path.iterdir()) == []


@pytest.mark.parametrize("fail_call", [1, 2])
def test_rng_failure_publishes_no_outputs(monkeypatch, capsys, fail_call):
    original = secrets.token_hex
    calls = 0

    def fail(n):
        nonlocal calls
        calls += 1
        if calls == fail_call:
            raise OSError("random source unavailable")
        return original(n)

    monkeypatch.setattr(generator.secrets, "token_hex", fail)
    with pytest.raises(OSError, match="random source unavailable"):
        generator.main()
    assert capsys.readouterr().out == ""


@pytest.mark.parametrize("shell", ["bash", "pwsh"])
@pytest.mark.parametrize(
    "case",
    ["valid", "missing", "unresolved", "newline", "short", "long", "invalid-generation"],
)
@pytest.mark.parametrize("marker", ["", "1"])
def test_import_validates_before_setting_marker(shell, case, marker):
    executable = BASH if shell == "bash" else PWSH
    if not executable:
        pytest.skip(f"{shell} is required")
    password = credential()
    env = dict(os.environ, SHARED_SQL_PASSWORD=password,
               SQL_CREDENTIAL_GENERATION=secrets.token_hex(16), SQL_PASSWORD_GENERATED=marker)
    if case == "missing":
        env.pop("SHARED_SQL_PASSWORD")
    elif case == "unresolved":
        env["SHARED_SQL_PASSWORD"] = "$(SHARED_SQL_PASSWORD)"
    elif case == "newline":
        env["SHARED_SQL_PASSWORD"] += "\n"
    elif case == "short":
        env["SHARED_SQL_PASSWORD"] = password[:-1]
    elif case == "long":
        env["SHARED_SQL_PASSWORD"] = password + secrets.token_hex(1)[0]
    elif case == "invalid-generation":
        env["SQL_CREDENTIAL_GENERATION"] = "$(SQL_CREDENTIAL_GENERATION)"
    command = (
        [executable, str(SCRIPTS / "import-sql-password.sh")]
        if shell == "bash" else
        [executable, "-NoProfile", "-File", str(SCRIPTS / "Import-SqlPassword.ps1")]
    )
    result = subprocess.run(command, env=env, capture_output=True, text=True)
    if case == "valid":
        assert result.returncode == 0, result.stderr
        assert result.stdout.splitlines() == [
            f"##vso[task.setvariable variable=SQL_PASSWORD;issecret=true]{password}",
            "##vso[task.setvariable variable=SQL_PASSWORD_GENERATED]1",
        ]
    else:
        assert result.returncode != 0
        assert "task.setvariable" not in result.stdout
        assert password not in result.stdout + result.stderr
        if case in ("short", "long"):
            assert env["SHARED_SQL_PASSWORD"] not in result.stdout + result.stderr


def evaluate(condition, context):
    """Evaluate only the small expression subset used by the stage gates."""
    tokens = iter(re.findall(r"'[^']*'|[^\s(),]+|[(),]", condition))
    functions = {
        "and": lambda *args: all(args), "or": lambda *args: any(args),
        "eq": lambda a, b: a == b, "ne": lambda a, b: a != b,
        "not": lambda a: not a, "canceled": lambda: context["canceled"],
        "succeeded": lambda stage=None: context.get(f"succeeded:{stage}", True),
    }

    def expression(token):
        if token not in functions:
            return context.get(token, token[1:-1] if token.startswith("'") else token)
        function = functions[token]
        assert next(tokens) == "("
        args = []
        token = next(tokens)
        while token != ")":
            args.append(expression(token))
            token = next(tokens)
            if token == ",":
                token = next(tokens)
        return function(*args)

    result = expression(next(tokens))
    assert next(tokens, None) is None
    return result


@pytest.mark.parametrize("reason", ["PullRequest", "IndividualCI"])
@pytest.mark.parametrize("mode", ["normal", "fuzz", "longhaul"])
@pytest.mark.parametrize("init_result", ["Succeeded", "Failed", "Skipped", "Canceled", "SucceededWithIssues"])
@pytest.mark.parametrize("duplicate", ["true", "false"])
@pytest.mark.parametrize("canceled", [False, True])
def test_stage_graph_fails_closed(reason, mode, init_result, duplicate, canceled):
    stages = {s["stage"]: s for s in template("validation-stages.yml")["stages"] if "stage" in s}
    context = {
        "variables['Build.Reason']": reason,
        "'${{ parameters.RunFuzz }}'": str(mode == "fuzz").lower(),
        "'${{ parameters.RunLongHaul }}'": str(mode == "longhaul").lower(),
        "dependencies.InitializeValidation.result": init_result,
        "dependencies.InitializeValidation.outputs['Initialize.SetDuplicateState.skipDuplicate']": duplicate,
        "canceled": canceled,
    }
    assert evaluate(stages["InitializeValidation"]["condition"], context) == (mode == "normal" and not canceled)
    allowed = mode == "normal" and init_result == "Succeeded" and not canceled
    for name in ("Build", "Build_mssql_python", "Kerberos_Test_PR"):
        expected = allowed and (reason != "PullRequest" or duplicate != "true")
        if name == "Kerberos_Test_PR":
            expected = expected and reason == "PullRequest"
        assert evaluate(stages[name]["condition"], context) == expected
    for name in ("Test_arm64", "Test_alpine_arm64"):
        assert stages[name]["dependsOn"] == ["Build", "InitializeValidation"]
        for build_succeeded in (False, True):
            context["succeeded:Build"] = build_succeeded and not canceled
            assert evaluate(stages[name]["condition"], context) == (
                allowed and reason != "PullRequest" and build_succeeded
            )


def test_initialization_steps_are_separate_and_fail_closed():
    init = template("validation-stages.yml")["stages"][0]
    assert init["stage"] == "InitializeValidation"
    job, = init["jobs"]
    assert job["job"] == "Initialize"
    assert job["pool"]["name"] == "${{ parameters.utilPoolName }}"
    duplicate, generate = job["steps"]
    assert duplicate["name"] == "SetDuplicateState"
    assert 'if [ "$BUILD_REASON" = "PullRequest" ]' in duplicate["bash"]
    assert "skipDuplicate;isOutput=true]false" in duplicate["bash"]
    assert duplicate["env"]["BUILD_REASON"] == "$(Build.Reason)"
    assert generate["name"] == "GenerateSqlCredential"
    assert generate["condition"] == "and(succeeded(), ne(variables['SetDuplicateState.skipDuplicate'], 'true'))"
    assert "set -euo pipefail" in generate["bash"]
    assert "||" not in generate["bash"]
    assert "env" not in generate
    assert not any(n.get("continueOnError") for n in nodes(init))


@pytest.mark.parametrize("reason", ["PullRequest", "IndividualCI", "BatchedCI", "Manual"])
def test_duplicate_lookup_is_pr_only(reason):
    step = template("validation-stages.yml")["stages"][0]["jobs"][0]["steps"][0]
    result = bash_run(
        'python3() { echo "PR_LOOKUP"; }\n' + step["bash"],
        {"BUILD_REASON": reason},
    )
    assert result.returncode == 0, result.stderr
    assert ("PR_LOOKUP" in result.stdout) == (reason == "PullRequest")
    assert ("skipDuplicate;isOutput=true]false" in result.stdout) == (reason != "PullRequest")


@pytest.mark.parametrize("duplicate", ["true", "false", ""])
@pytest.mark.parametrize("success", [True, False])
def test_generation_requires_success_and_nonduplicate(duplicate, success):
    step = template("validation-stages.yml")["stages"][0]["jobs"][0]["steps"][1]
    assert evaluate(step["condition"], {
        "variables['SetDuplicateState.skipDuplicate']": duplicate,
        "succeeded:None": success,
    }) == (success and duplicate != "true")


def test_all_cross_pool_jobs_map_only_initialization_outputs():
    files = [
        "sql-host-template.yml", "test-matrix-template-arm64.yml",
        "test-matrix-template-alpine_arm64.yml", "test-matrix-odbc-template-arm64.yml",
    ]
    jobs = [n for name in files for n in nodes(template(name)) if "job" in n]
    assert len(jobs) == 7
    build = next(s for s in template("validation-stages.yml")["stages"] if s.get("stage") == "Build")
    for job in build["jobs"]:
        if job.get("job") in ("Build_Linux_ARM", "Build_Windows_Arm"):
            variables = job["variables"]["${{ if eq(variables['Build.Reason'], 'PullRequest') }}"]
            jobs.append(dict(job, variables=variables))
        else:
            assert "SHARED_SQL_PASSWORD" not in str(job.get("variables", {}))
    for job in jobs:
        assert "dependsOn" not in job  # No test waits for its long-lived SQL host.
        for variable, output in (("SHARED_SQL_PASSWORD", "SQL_PASSWORD"),
                                 ("SQL_CREDENTIAL_GENERATION", "SQL_CREDENTIAL_GENERATION")):
            assert job["variables"][variable] == (
                f"$[ stageDependencies.InitializeValidation.Initialize.outputs['GenerateSqlCredential.{output}'] ]"
            )
    for stage in template("validation-stages.yml")["stages"]:
        calls = {n["template"] for n in nodes(stage.get("jobs", [])) if "template" in n}
        if calls.intersection(files):
            assert "InitializeValidation" in stage["dependsOn"]
            assert "eq(dependencies.InitializeValidation.result, 'Succeeded')" in stage["condition"]


def test_consumer_env_artifact_names_and_no_secret_script_interpolation():
    names = [
        "sql-host-template.yml", "test-matrix-template-arm64.yml",
        "test-matrix-template-alpine_arm64.yml", "test-matrix-odbc-template-arm64.yml",
        "build-template-container.yml", "odbc-e2e-windows-remote-sql-template.yml",
    ]
    imports = polls = 0
    for name in names:
        for node in nodes(template(name)):
            script = node.get("bash", node.get("pwsh", node.get("script", "")))
            assert "$(SQL_PASSWORD)" not in script
            assert "$(SHARED_SQL_PASSWORD)" not in script
            if "import-sql-password.sh" in script or "Import-SqlPassword.ps1" in script:
                imports += 1
                assert node["env"] == {
                    "SHARED_SQL_PASSWORD": "$(SHARED_SQL_PASSWORD)",
                    "SQL_CREDENTIAL_GENERATION": "$(SQL_CREDENTIAL_GENERATION)",
                }
            if ("bash .pipeline/scripts/sql-host/poll-for-endpoint.sh" in script
                    or ".pipeline/scripts/sql-host/Wait-ForSqlEndpoint.ps1" in script):
                polls += 1
                assert node["env"]["SQL_CREDENTIAL_GENERATION"] == "$(SQL_CREDENTIAL_GENERATION)"
                assert node["env"]["ARTIFACT_NAME"].endswith(SUFFIX)
            if (node.get("task") == "PublishPipelineArtifact@1"
                    and "sentinel" in node.get("displayName", "")):
                assert node["inputs"]["artifact"].endswith(SUFFIX)
                assert "PASSWORD" not in str(node)
            if "SENTINEL_SUFFIX" in node:
                assert node["SENTINEL_SUFFIX"] == SUFFIX
    assert imports == 9 and polls == 8
    assert not (SCRIPTS / "derive-sql-password.sh").exists()
    assert not (SCRIPTS / "New-SqlPassword.ps1").exists()
    for path in TEMPLATES.glob("*.yml"):
        text = path.read_text(encoding="utf-8")
        if "import-sql-password.sh" in text or "Import-SqlPassword.ps1" in text:
            assert path.name in names
    build = template("validation-stages.yml")
    assert "scripts/test_shared_sql_credentials.py" in str(build)


def bash_run(script, env, cwd=ROOT):
    if not BASH:
        pytest.skip("Bash is required")
    python = Path(sys.executable).as_posix()
    return subprocess.run(
        [BASH, "-s"], input=f'python3() {{ "{python}" "$@"; }}\n' + script,
        cwd=cwd, env=dict(os.environ, **env), capture_output=True, text=True, timeout=20,
    )


def test_consuming_stage_retries_reuse_original_output():
    password, generation = credential(), secrets.token_hex(16)
    outputs = []
    for attempt in (1, 2):
        result = bash_run("source .pipeline/scripts/sql-host/import-sql-password.sh\n", {
            "SHARED_SQL_PASSWORD": password, "SQL_CREDENTIAL_GENERATION": generation,
            "SYSTEM_STAGEATTEMPT": str(attempt), "SQL_PASSWORD_GENERATED": "",
        })
        assert result.returncode == 0, result.stderr
        outputs.append(result.stdout)
    assert outputs[0] == outputs[1]


def test_teardown_ignores_prior_generation_and_stage_attempt(tmp_path):
    generation, old_generation = secrets.token_hex(16), secrets.token_hex(16)
    counter = tmp_path / "polls"
    counter.write_text("0", encoding="utf-8")
    result = bash_run(r'''
curl() {
    count=$(cat "$TEST_COUNTER")
    count=$((count + 1))
    echo "$count" > "$TEST_COUNTER"
    case "$count" in
        1) suffix="-$OLD_GENERATION-2" ;;
        2) suffix="-$SQL_CREDENTIAL_GENERATION-1" ;;
        *) suffix="$SENTINEL_SUFFIX" ;;
    esac
    printf '{"value":[{"name":"test%s"}]}' "$suffix"
}
sleep() { :; }
source .pipeline/scripts/sql-host/wait-for-teardown.sh
''', {
        "TEST_COUNTER": counter.as_posix(), "OLD_GENERATION": old_generation,
        "SQL_CREDENTIAL_GENERATION": generation, "SENTINEL_SUFFIX": f"-{generation}-2",
        "EXPECTED_SENTINELS": "test", "BUILD_BUILDID": "1",
        "SYSTEM_COLLECTIONURI": "https://example.invalid/", "SYSTEM_TEAMPROJECT": "test",
        "SYSTEM_ACCESSTOKEN": secrets.token_hex(16), "MAX_LIFETIME_SECONDS": "10",
    })
    assert result.returncode == 0, result.stderr
    assert "All 1 teardown sentinel(s) present" in result.stdout
    assert counter.read_text().strip() == "3"


@pytest.mark.parametrize("shell", ["bash", "pwsh"])
@pytest.mark.parametrize("case", ["current", "stale", "legacy", "expired", "missing-generation"])
def test_endpoint_generation_and_deadline_guard(tmp_path, shell, case):
    generation = secrets.token_hex(16)
    endpoint_generation = secrets.token_hex(16) if case == "stale" else generation
    deadline = int(time.time()) + (-10 if case == "expired" else 1800)
    payload = f"127.0.0.1 14333 {deadline}"
    if case != "legacy":
        payload += f" {endpoint_generation}"
    artifact = tmp_path / "endpoint.zip"
    with zipfile.ZipFile(artifact, "w") as archive:
        archive.writestr("endpoint.txt", payload + "\n")
    env = dict(
        ARTIFACT_NAME=f"sql-ready-test-{generation}-2", SQL_CREDENTIAL_GENERATION=generation,
        BUILD_BUILDID="1", SYSTEM_COLLECTIONURI="https://example.invalid/",
        SYSTEM_TEAMPROJECT="test", SYSTEM_ACCESSTOKEN=secrets.token_hex(16),
        AGENT_TEMPDIRECTORY=tmp_path.as_posix(), TEST_ZIP=artifact.as_posix(),
        SYSTEM_STAGEATTEMPT="2",
    )
    if case == "missing-generation":
        env["SQL_CREDENTIAL_GENERATION"] = ""
    if shell == "bash":
        script = r'''
curl() {
    while [ "$#" -gt 0 ]; do
        if [ "$1" = "-o" ]; then cp "$TEST_ZIP" "$2"; return; fi
        shift
    done
    printf '{"value":[{"name":"%s","resource":{"downloadUrl":"https://example.invalid/endpoint"}}]}' "$ARTIFACT_NAME"
}
source .pipeline/scripts/sql-host/poll-for-endpoint.sh
'''
        result = bash_run(script, env)
    else:
        if not PWSH:
            pytest.skip("PowerShell is required")
        script = """
function Invoke-RestMethod {
    @{ value = @(@{ name = $env:ARTIFACT_NAME; resource = @{ downloadUrl = 'https://example.invalid/endpoint' } }) }
}
function Invoke-WebRequest { param($Uri, $Headers, $OutFile, [switch]$UseBasicParsing)
    Copy-Item $env:TEST_ZIP $OutFile
}
& ./.pipeline/scripts/sql-host/Wait-ForSqlEndpoint.ps1
"""
        result = subprocess.run(
            [PWSH, "-NoProfile", "-Command", script], cwd=ROOT,
            env=dict(os.environ, **env), capture_output=True, text=True, timeout=20,
        )
    assert (result.returncode == 0) == (case == "current"), result.stderr
    assert ("task.setvariable variable=DB_HOST" in result.stdout) == (case == "current")


def test_host_payload_contains_no_secret(tmp_path):
    if not BASH:
        pytest.skip("Bash is required")
    script_dir = tmp_path / "scripts"
    script_dir.mkdir()
    cert_script = script_dir / "generate_cert.sh"
    cert_script.write_text("#!/usr/bin/env bash\ntouch ca.crt mssql.crt mssql.pem mssql.key\n", encoding="utf-8")
    cert_script.chmod(0o755)
    password, generation = credential(), secrets.token_hex(16)
    env = dict(
        INSTANCE_ID="test", SQL_PASSWORD=password, SQL_CREDENTIAL_GENERATION=generation,
        SQL_IMAGE_TAG="test", BUILD_ARTIFACTSTAGINGDIRECTORY=(tmp_path / "artifacts").as_posix(),
    )
    result = bash_run(
        'docker() { return 0; }\nhostname() { echo 127.0.0.1; }\n'
        'sudo() { if [ "$1" != chown ]; then "$@"; fi; }\n'
        f'source "{(SCRIPTS / "start.sh").as_posix()}"\n', env, cwd=tmp_path,
    )
    assert result.returncode == 0, result.stderr
    assert password not in result.stdout + result.stderr
    files = list((tmp_path / "artifacts" / "sql-endpoint-test").iterdir())
    assert {p.name for p in files} == {"endpoint.txt", "ca.crt", "mssql.crt", "mssql.pem"}
    for path in files:
        assert password not in path.read_text(encoding="utf-8")
    fields = (tmp_path / "artifacts" / "sql-endpoint-test" / "endpoint.txt").read_text().split()
    assert fields[3] == generation

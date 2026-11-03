# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""SQL credential key-exchange contracts.

The host and consumer scripts run for real against a fake agent that turns
`artifact.upload` logging commands into a local artifact store. Fork-build
artifact behavior and service-side masking still need a live ADO run.
"""

import importlib.util
import os
from functools import lru_cache
from pathlib import Path
import re
import secrets
import shutil
import string
import subprocess
import sys
import tempfile
import threading
import time

import pytest
import yaml

ROOT = Path(__file__).parents[1]
TEMPLATES = ROOT / ".pipeline" / "templates"
SCRIPTS = ROOT / ".pipeline" / "scripts" / "sql-host"
BASH = shutil.which("bash")
PWSH = shutil.which("pwsh")
SPEC = importlib.util.spec_from_file_location(
    "credential_generation", SCRIPTS / "generate-credential-generation.py")
generator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generator)
SUFFIX = "-$(SQL_CREDENTIAL_GENERATION)-$(System.StageAttempt)"
GENERATION_OUTPUT = (
    "$[ stageDependencies.InitializeValidation.Initialize.outputs"
    "['GenerateSqlCredential.SQL_CREDENTIAL_GENERATION'] ]"
)
PYTHON = Path(sys.executable).as_posix()
OAEP = ("-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha256",
        "-pkeyopt", "rsa_mgf1_md:sha256")
UPLOAD = re.compile(r"##vso\[artifact\.upload containerfolder=([^;\]]*);artifactname=([^\]]*)\](.*)")

# Serves the build artifacts REST API from FAKE_STORE: one directory per artifact.
STORE_TOOL = r'''
import json, os, sys, zipfile
store = os.environ["FAKE_STORE"]
if sys.argv[1] == "list":
    names = sorted(n for n in os.listdir(store) if not n.startswith("."))
    value = [{"name": n, "resource": {"downloadUrl": "https://example.invalid/download/" + n}}
             for n in names]
    print(json.dumps({"value": value}, separators=(",", ":")))
    sys.exit()
name, out = sys.argv[2:4]
root = os.path.join(store, name)
if name.startswith(".") or not os.path.isdir(root):
    sys.exit(22)
with zipfile.ZipFile(out, "w") as archive:
    for base, _, files in os.walk(root):
        for file in files:
            path = os.path.join(base, file)
            archive.write(path, name + "/" + os.path.relpath(path, root).replace(os.sep, "/"))
'''

BASH_MOCKS = r'''
python3() { "$TEST_PYTHON" "$@" | tr -d '\r'; }
curl() {
    local out="" url=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -o) out="$2"; shift 2 ;;
            *) url="$1"; shift ;;
        esac
    done
    if [ -n "$out" ]; then
        python3 "$FAKE_STORE_TOOL" zip "${url##*/}" "$out"
    else
        python3 "$FAKE_STORE_TOOL" list
    fi
}
'''

PWSH_MOCKS = r'''
function Invoke-RestMethod { param($Uri, $Headers, $Method)
    & $env:TEST_PYTHON $env:FAKE_STORE_TOOL list | ConvertFrom-Json
}
function Invoke-WebRequest { param($Uri, $Headers, $OutFile, [switch]$UseBasicParsing)
    & $env:TEST_PYTHON $env:FAKE_STORE_TOOL zip ($Uri -split '/')[-1] $OutFile
    if ($LASTEXITCODE) { throw "download failed: $Uri" }
}
'''

ENCRYPT_PS = r'''
$rsa = [Security.Cryptography.RSA]::Create()
$rsa.ImportFromPem([IO.File]::ReadAllText($env:TEST_KEY))
$cipher = $rsa.Encrypt([IO.File]::ReadAllBytes($env:TEST_IN), [Security.Cryptography.RSAEncryptionPadding]::OaepSHA256)
[IO.File]::WriteAllBytes($env:TEST_OUT, $cipher)
'''


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
    alphabet = string.ascii_letters + string.digits
    return "Aa1!" + "".join(secrets.choice(alphabet) for _ in range(22))


@lru_cache
def bash_has_openssl():
    return bool(BASH) and subprocess.run(
        [BASH, "-c", "command -v openssl"], capture_output=True).returncode == 0


def require_bash_openssl():
    if not bash_has_openssl():
        pytest.skip("Bash with OpenSSL is required")


def openssl(*args):
    require_bash_openssl()
    return subprocess.run([BASH, "-c", 'openssl "$@"', "openssl", *args],
                          capture_output=True, timeout=60)


def public_key(directory, bits):
    private, public = directory / f"private-{bits}.pem", directory / f"public-{bits}.pem"
    assert openssl("genpkey", "-algorithm", "RSA", "-pkeyopt", f"rsa_keygen_bits:{bits}",
                   "-out", private.as_posix()).returncode == 0
    assert openssl("pkey", "-in", private.as_posix(), "-pubout", "-out", public.as_posix()).returncode == 0
    private.unlink()
    return public.read_bytes()


def encrypt(key, plaintext):
    """Encrypt as wait-for-teardown.sh does; use .NET where Bash lacks OpenSSL."""
    with tempfile.TemporaryDirectory() as work:
        source, target = Path(work) / "plain", Path(work) / "cipher"
        source.write_bytes(plaintext)
        if bash_has_openssl():
            result = openssl("pkeyutl", "-encrypt", "-pubin", "-inkey", key.as_posix(), *OAEP,
                             "-in", source.as_posix(), "-out", target.as_posix())
        else:
            result = subprocess.run(
                [PWSH, "-NoProfile", "-Command", ENCRYPT_PS], capture_output=True, timeout=60,
                env=dict(os.environ, TEST_KEY=str(key), TEST_IN=str(source), TEST_OUT=str(target)),
            )
        assert result.returncode == 0, result.stderr
        return target.read_bytes()


class Run:
    def __init__(self, agent, process):
        self.process = process
        self.stdout, self.stderr = [], []
        self.threads = [
            threading.Thread(target=self._read, args=(process.stdout, self.stdout, agent), daemon=True),
            threading.Thread(target=self._read, args=(process.stderr, self.stderr, None), daemon=True),
        ]
        for thread in self.threads:
            thread.start()

    @staticmethod
    def _read(stream, lines, agent):
        for line in stream:
            lines.append(line.rstrip("\r\n"))
            if agent:
                try:
                    agent.handle(line.rstrip("\r\n"))
                except BaseException as error:
                    agent.errors.append(repr(error))

    @property
    def output(self):
        return "\n".join(self.stdout + self.stderr)

    def logged(self, prefix):
        return any(line.startswith(prefix) for line in self.stdout)

    def wait(self, timeout=120):
        try:
            self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
            for thread in self.threads:
                thread.join(timeout=5)
            pytest.fail(f"timed out after {timeout}s:\n{self.output}")
        for thread in self.threads:
            thread.join(timeout=10)
        return self.process.returncode


class FakeAgent:
    """Stands in for the agent's artifact.upload handling and the build artifacts API."""

    def __init__(self, root):
        self.root = root
        self.store = root / "store"
        self.store.mkdir()
        self.tool = root / "fake_store.py"
        self.tool.write_text(STORE_TOOL, encoding="utf-8")
        self.uploads, self.errors, self.runs = [], [], []
        self.on_upload = None
        self._lock = threading.Lock()

    def publish(self, name, files):
        staging = self.store / f".staging-{name}"
        staging.mkdir()
        for file, content in files.items():
            (staging / file).write_bytes(content.encode() if isinstance(content, str) else content)
        os.rename(staging, self.store / name)

    def handle(self, line):
        match = UPLOAD.search(line)
        if not match:
            return
        folder, name, path = match.groups()
        with self._lock:
            # Azure Pipelines fails a step that reuses an artifact name.
            if folder != name or name in self.uploads or (self.store / name).exists():
                self.errors.append(f"rejected upload {name!r}")
                return
            self.uploads.append(name)
        staging = self.store / f".staging-{name}"
        shutil.copytree(path.strip(), staging)
        os.rename(staging, self.store / name)
        if self.on_upload:
            self.on_upload(name)

    def serve(self, plaintext):
        def answer(name):
            if name.startswith("sql-key-"):
                ciphertext = encrypt(self.store / name / "key.pem", plaintext)
                self.publish("sql-cred-" + name[len("sql-key-"):], {"credential.bin": ciphertext})
        self.on_upload = answer

    def start(self, shell, script, temp, values):
        temp.mkdir()
        env = dict(
            os.environ, FAKE_STORE=str(self.store), FAKE_STORE_TOOL=self.tool.as_posix(),
            TEST_PYTHON=PYTHON, AGENT_TEMPDIRECTORY=temp.as_posix(),
            BUILD_SOURCESDIRECTORY=temp.as_posix(), BUILD_BUILDID="1",
            SYSTEM_COLLECTIONURI="https://example.invalid/", SYSTEM_TEAMPROJECT="test",
            SYSTEM_ACCESSTOKEN=secrets.token_hex(16), POLL_INTERVAL_SECONDS="1",
        )
        env.update(values)
        if shell == "bash":
            path = self.root / f"run-{secrets.token_hex(4)}.sh"
            path.write_text(BASH_MOCKS + script, encoding="utf-8", newline="\n")
            command = [BASH, path.as_posix()]
        else:
            command = [PWSH, "-NoProfile", "-Command", PWSH_MOCKS + script]
        process = subprocess.Popen(
            command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", errors="replace",
        )
        run = Run(self, process)
        self.runs.append(run)
        return run

    def close(self):
        for run in self.runs:
            if run.process.poll() is None:
                run.process.kill()
                run.process.wait()


@pytest.fixture
def agent(tmp_path):
    fake = FakeAgent(tmp_path)
    yield fake
    fake.close()


def publish_endpoint(agent, generation, payload=None):
    payload = payload or f"127.0.0.1 14333 {int(time.time()) + 1800} {generation}"
    agent.publish(f"sql-ready-test-{generation}-2", {"endpoint.txt": payload + "\n"})


def start_host(agent, password, generation, **values):
    require_bash_openssl()
    values = {"EXPECTED_SENTINELS": "test", "SENTINEL_SUFFIX": f"-{generation}-2",
              "SQL_PASSWORD": password, "MAX_LIFETIME_SECONDS": "120", **values}
    return agent.start("bash", "source .pipeline/scripts/sql-host/wait-for-teardown.sh\n",
                       agent.root / "host", values)


def start_consumer(agent, shell, generation, **values):
    if shell == "bash":
        require_bash_openssl()
        script = "source .pipeline/scripts/sql-host/poll-for-endpoint.sh\n"
    else:
        if not PWSH:
            pytest.skip("PowerShell is required")
        script = "& ./.pipeline/scripts/sql-host/Wait-ForSqlEndpoint.ps1\n"
    values = dict(
        dict(ARTIFACT_NAME=f"sql-ready-test-{generation}-2", SENTINEL_ARTIFACT_NAME=f"test-{generation}-2",
             SQL_CREDENTIAL_GENERATION=generation, SYSTEM_STAGEATTEMPT="2", CREDENTIAL_WAIT_SECONDS="60"),
        **values,
    )
    return agent.start(shell, script, agent.root / f"consumer-{shell}", values)


def test_native_shell_is_available():
    if sys.platform == "win32":
        assert PWSH, "PowerShell is required for Windows credential regression tests"
    elif sys.platform.startswith("linux"):
        assert BASH, "Bash is required for Linux credential regression tests"
        assert bash_has_openssl(), "OpenSSL is required for Linux credential regression tests"


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


def test_generator_emits_only_nonsecret_generation(capsys, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    prefix = "##vso[task.setvariable variable=SQL_CREDENTIAL_GENERATION;isOutput=true]"
    generations = set()
    for _ in range(64):
        generator.main()
        line, = capsys.readouterr().out.splitlines()
        assert line.startswith(prefix)
        generation = line[len(prefix):]
        assert re.fullmatch(r"[0-9a-f]{32}", generation)
        generations.add(generation)
    assert len(generations) == 64
    assert list(tmp_path.iterdir()) == []


def test_rng_failure_publishes_no_outputs(monkeypatch, capsys):
    def fail(n):
        raise OSError("random source unavailable")

    monkeypatch.setattr(generator.secrets, "token_hex", fail)
    with pytest.raises(OSError, match="random source unavailable"):
        generator.main()
    assert capsys.readouterr().out == ""


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
    assert ".pipeline/scripts/sql-host/generate-credential-generation.py" in generate["bash"]
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


def test_all_cross_pool_jobs_map_only_the_generation():
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
    assert len(jobs) == 9
    for job in jobs:
        assert "dependsOn" not in job  # No test waits for its long-lived SQL host.
        assert job["variables"]["SQL_CREDENTIAL_GENERATION"] == GENERATION_OUTPUT
        assert not [name for name in job["variables"] if "PASSWORD" in name]
    for path in TEMPLATES.glob("*.yml"):
        text = path.read_text(encoding="utf-8")
        assert "GenerateSqlCredential.SQL_PASSWORD" not in text, path.name
        assert "SHARED_SQL_PASSWORD" not in text, path.name
    for stage in template("validation-stages.yml")["stages"]:
        calls = {n["template"] for n in nodes(stage.get("jobs", [])) if "template" in n}
        if calls.intersection(files):
            assert "InitializeValidation" in stage["dependsOn"]
            assert "eq(dependencies.InitializeValidation.result, 'Succeeded')" in stage["condition"]


def is_poll(node):
    script = node.get("bash", node.get("pwsh", node.get("script", "")))
    return ("bash .pipeline/scripts/sql-host/poll-for-endpoint.sh" in script
            or ".pipeline/scripts/sql-host/Wait-ForSqlEndpoint.ps1" in script)


def is_teardown_publisher(node):
    return (node.get("task") == "PublishPipelineArtifact@1"
            and "teardown sentinel" in node.get("displayName", ""))


def test_each_consumer_names_its_key_after_its_teardown_sentinel():
    names = [
        "sql-host-template.yml", "test-matrix-template-arm64.yml",
        "test-matrix-template-alpine_arm64.yml", "test-matrix-odbc-template-arm64.yml",
        "build-template-container.yml", "odbc-e2e-windows-remote-sql-template.yml",
    ]
    polls = paired = 0
    for name in names:
        for node in nodes(template(name)):
            script = node.get("bash", node.get("pwsh", node.get("script", "")))
            assert "$(SQL_PASSWORD)" not in script
            if is_poll(node):
                polls += 1
                env = node["env"]
                assert env["SQL_CREDENTIAL_GENERATION"] == "$(SQL_CREDENTIAL_GENERATION)"
                assert env["ARTIFACT_NAME"].endswith(SUFFIX)
                assert env["SENTINEL_ARTIFACT_NAME"].endswith(SUFFIX)
                assert not [key for key in env if "PASSWORD" in key]
            if is_teardown_publisher(node):
                assert node["inputs"]["artifact"].endswith(SUFFIX)
                assert "PASSWORD" not in str(node)
            if "SENTINEL_SUFFIX" in node:
                assert node["SENTINEL_SUFFIX"] == SUFFIX
            if "steps" in node:
                steps = list(nodes(node["steps"]))
                consumers = [n for n in steps if is_poll(n)]
                if consumers:
                    publishers = [n for n in steps if is_teardown_publisher(n)]
                    assert len(consumers) == len(publishers) == 1, name
                    assert consumers[0]["env"]["SENTINEL_ARTIFACT_NAME"] == publishers[0]["inputs"]["artifact"]
                    paired += 1
    assert polls == paired == 8
    for retired in ("generate-shared-password.py", "import-sql-password.sh", "Import-SqlPassword.ps1",
                    "derive-sql-password.sh", "New-SqlPassword.ps1"):
        assert not (SCRIPTS / retired).exists()
        for path in TEMPLATES.glob("*.yml"):
            assert retired not in path.read_text(encoding="utf-8"), path.name
    assert "scripts/test_shared_sql_credentials.py" in str(template("validation-stages.yml"))


def test_sql_host_password_stays_job_local():
    job, = template("sql-host-template.yml")["jobs"]
    assert job["steps"][0] == {"template": "generate-sql-password-template.yml",
                               "parameters": {"osType": "Linux"}}
    assert not [name for name in job["variables"] if "PASSWORD" in name]
    serve = next(step for step in job["steps"] if "wait-for-teardown.sh" in step.get("script", ""))
    assert serve["env"]["SQL_PASSWORD"] == "$(SQL_PASSWORD)"
    assert serve["env"]["AGENT_TEMPDIRECTORY"] == "$(Agent.TempDirectory)"
    assert serve["env"]["SENTINEL_SUFFIX"] == "-$(SQL_CREDENTIAL_GENERATION)-$(System.StageAttempt)"
    generate = (TEMPLATES / "generate-sql-password-template.yml").read_text(encoding="utf-8")
    assert "isOutput" not in generate


def bash_run(script, env, cwd=ROOT):
    if not BASH:
        pytest.skip("Bash is required")
    return subprocess.run(
        [BASH, "-s"], input=f'python3() {{ "{PYTHON}" "$@"; }}\n' + script,
        cwd=cwd, env=dict(os.environ, **env), capture_output=True, text=True, timeout=20,
    )


def test_teardown_ignores_prior_generation_and_stage_attempt(tmp_path):
    require_bash_openssl()
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
        "EXPECTED_SENTINELS": "test", "BUILD_BUILDID": "1", "SQL_PASSWORD": credential(),
        "SYSTEM_COLLECTIONURI": "https://example.invalid/", "SYSTEM_TEAMPROJECT": "test",
        "SYSTEM_ACCESSTOKEN": secrets.token_hex(16), "MAX_LIFETIME_SECONDS": "10",
        "AGENT_TEMPDIRECTORY": tmp_path.as_posix(),
    })
    assert result.returncode == 0, result.stderr
    assert "All 1 teardown sentinel(s) present" in result.stdout
    assert counter.read_text().strip() == "3"


@pytest.mark.parametrize("shell", ["bash", "pwsh"])
def test_key_exchange_delivers_password_only_to_consumer(agent, shell):
    password, generation = credential(), secrets.token_hex(16)
    sentinel = f"test-{generation}-2"
    publish_endpoint(agent, generation)
    host = start_host(agent, password, generation)
    consumer = start_consumer(agent, shell, generation)
    assert consumer.wait() == 0, consumer.output
    agent.publish(sentinel, {"done.txt": "done\n"})
    assert host.wait() == 0, host.output
    assert agent.errors == []
    assert sorted(agent.uploads) == [f"sql-cred-{sentinel}", f"sql-key-{sentinel}"]
    assert f"##vso[task.setvariable variable=SQL_PASSWORD;issecret=true]{password}" in consumer.stdout
    assert "##vso[task.setvariable variable=SQL_PASSWORD_GENERATED]1" in consumer.stdout
    assert "##vso[task.setvariable variable=DB_HOST]127.0.0.1" in consumer.stdout
    assert consumer.output.count(password) == 1
    assert password not in host.output
    assert f"Served SQL credential to {sentinel}." in host.stdout
    key = (agent.store / f"sql-key-{sentinel}" / "key.pem").read_text(encoding="ascii")
    assert key.startswith("-----BEGIN PUBLIC KEY-----")
    assert not list((agent.root / f"consumer-{shell}").glob("sql-key-private.*"))
    for path in agent.root.rglob("*"):
        if path.is_file():
            data = path.read_bytes()
            assert password.encode() not in data, path
            assert b"PRIVATE KEY" not in data, path


@pytest.mark.parametrize("case", ["rsa2048", "garbage", "missing"])
def test_host_rejects_unusable_key(agent, case):
    password, generation = credential(), secrets.token_hex(16)
    sentinel = f"test-{generation}-2"
    files = {"key.pem": "-----BEGIN PUBLIC KEY-----\nnot a key\n-----END PUBLIC KEY-----\n"}
    if case == "rsa2048":
        files = {"key.pem": public_key(agent.root, 2048)}
    elif case == "missing":
        files = {"other.pem": public_key(agent.root, 3072)}
    agent.publish(f"sql-key-{sentinel}", files)
    agent.publish(sentinel, {"done.txt": "done\n"})
    host = start_host(agent, password, generation)
    assert host.wait() == 0, host.output
    assert agent.uploads == [] and agent.errors == []
    assert host.logged(f"##vso[task.logissue type=warning]sql-key-{sentinel} "), host.output
    assert password not in host.output


@pytest.mark.parametrize("case", ["missing-password", "short-password", "unsafe-sentinel"])
def test_host_rejects_invalid_configuration(agent, case):
    password, generation = credential(), secrets.token_hex(16)
    values = {}
    if case == "missing-password":
        password = ""
    elif case == "short-password":
        password = password[:-1]
    else:
        values["EXPECTED_SENTINELS"] = "test;x"
    host = start_host(agent, password, generation, **values)
    assert host.wait() == 2, host.output
    assert agent.uploads == []
    assert not password or password not in host.output


@pytest.mark.parametrize("shell", ["bash", "pwsh"])
@pytest.mark.parametrize("case", [
    "injected", "trailing-newline", "tampered", "plaintext", "missing-file", "missing",
])
def test_consumer_rejects_unusable_credential(agent, shell, case):
    password, generation = credential(), secrets.token_hex(16)
    sentinel = f"test-{generation}-2"
    publish_endpoint(agent, generation)

    def answer(name):
        if name != f"sql-key-{sentinel}" or case == "missing":
            return
        key = agent.store / name / "key.pem"
        if case == "injected":
            injected = password.encode() + b"\n##vso[task.setvariable variable=DB_HOST]injected"
            files = {"credential.bin": encrypt(key, injected)}
        elif case == "trailing-newline":
            files = {"credential.bin": encrypt(key, password.encode() + b"\n")}
        elif case == "tampered":
            ciphertext = bytearray(encrypt(key, password.encode()))
            ciphertext[-1] ^= 1
            files = {"credential.bin": bytes(ciphertext)}
        elif case == "plaintext":
            files = {"credential.bin": password}
        else:
            files = {"other.bin": encrypt(key, password.encode())}
        agent.publish(f"sql-cred-{sentinel}", files)

    agent.on_upload = answer
    consumer = start_consumer(agent, shell, generation, CREDENTIAL_WAIT_SECONDS="3")
    assert consumer.wait() != 0, consumer.output
    assert agent.errors == []
    assert "variable=SQL_PASSWORD" not in consumer.output
    assert "variable=DB_HOST" not in consumer.output
    assert password not in consumer.output
    if case == "missing":
        assert consumer.logged(f"##vso[task.logissue type=error]SQL credential sql-cred-{sentinel} "), consumer.output


@pytest.mark.parametrize("shell", ["bash", "pwsh"])
@pytest.mark.parametrize("case", [
    "current", "stale", "legacy", "expired", "missing-generation", "wrong-attempt", "unsafe-sentinel",
])
def test_endpoint_generation_and_deadline_guard(agent, shell, case):
    password, generation = credential(), secrets.token_hex(16)
    endpoint_generation = secrets.token_hex(16) if case == "stale" else generation
    deadline = int(time.time()) + (-10 if case == "expired" else 1800)
    payload = f"127.0.0.1 14333 {deadline}"
    if case != "legacy":
        payload += f" {endpoint_generation}"
    publish_endpoint(agent, generation, payload)
    agent.serve(password.encode())
    values = {}
    if case == "missing-generation":
        values["SQL_CREDENTIAL_GENERATION"] = ""
    elif case == "wrong-attempt":
        values["SENTINEL_ARTIFACT_NAME"] = f"test-{generation}-1"
    elif case == "unsafe-sentinel":
        values["SENTINEL_ARTIFACT_NAME"] = f"test;x-{generation}-2"
    consumer = start_consumer(agent, shell, generation, **values)
    assert (consumer.wait() == 0) == (case == "current"), consumer.output
    assert agent.errors == []
    assert ("variable=DB_HOST" in consumer.output) == (case == "current")
    assert ("variable=SQL_PASSWORD;issecret=true" in consumer.output) == (case == "current")
    if case in ("missing-generation", "wrong-attempt", "unsafe-sentinel"):
        assert agent.uploads == []


@pytest.mark.parametrize("shell", ["bash", "pwsh"])
def test_endpoint_lifetime_is_rechecked_after_credential_wait(agent, shell):
    password, generation = credential(), secrets.token_hex(16)
    deadline = int(time.time()) + 10
    publish_endpoint(agent, generation, f"127.0.0.1 14333 {deadline} {generation}")
    agent.serve(password.encode())
    answer = agent.on_upload

    def answer_after_endpoint_expires(name):
        if name.startswith("sql-key-"):
            while time.time() <= deadline:
                time.sleep(0.1)
        answer(name)

    agent.on_upload = answer_after_endpoint_expires
    consumer = start_consumer(
        agent, shell, generation, MIN_HOST_LIFE_REMAINING_SECONDS="3",
        CREDENTIAL_WAIT_SECONDS="30",
    )
    assert consumer.wait() != 0, consumer.output
    assert agent.errors == []
    assert f"SQL credential sql-cred-test-{generation}-2 found." in consumer.output
    assert "endpoint expires in" in consumer.output
    assert "variable=SQL_PASSWORD" not in consumer.output
    assert "variable=DB_HOST" not in consumer.output
    assert "variable=DB_PORT" not in consumer.output


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

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Execute the GHCR pipeline scripts with environment inputs and mocked registries."""

import os
import re
import secrets
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
import yaml

_ROOT = Path(__file__).parents[1]
_PIPELINE = yaml.safe_load(
    (_ROOT / ".pipeline" / "sync-container-images.yml").read_text(encoding="utf-8")
)
_STAGE = next(s for s in _PIPELINE["stages"] if s["stage"] == "Mirror_To_GHCR")
_STEPS = [s for s in _STAGE["jobs"][0]["steps"] if "displayName" in s]
_BASH = shutil.which("bash")
_ENABLED = "and(succeeded(), eq(variables['GHCR_ENABLED'], 'true'))"
_PAYLOADS = [
    '"; touch injected; #',
    "'; touch injected; #",
    "$(touch injected)",
    "`touch injected`",
    "owner\n touch injected\n",
    "owner\r\n##vso[task.setvariable variable=GHCR_ENABLED]true",
    "--help",
    "-u",
    "${IFS}touch${IFS}injected",
]
_MOCKS = r"""
docker() {
  printf 'docker\t' >> calls
  printf '%s\t' "$@" >> calls
  printf '\n' >> calls
  if [[ "$1" == login ]]; then
    actual="$(cat)"
    [[ "$actual" == "$EXPECTED_TOKEN" ]] || return 91
    return "${LOGIN_EXIT:-0}"
  fi
  return "${MIRROR_EXIT:-0}"
}
az() {
  printf 'az\t' >> calls
  printf '%s\t' "$@" >> calls
  printf '\n' >> calls
}
"""


def _expand(text, parameters, variables):
    text = re.sub(
        r"\$\{\{\s*parameters\.(\w+)\s*\}\}",
        lambda m: parameters[m[1]], text,
    )
    return re.sub(r"\$\(([\w.]+)\)", lambda m: variables.get(m[1], m[0]), text)


@pytest.fixture
def run_pipeline(tmp_path):
    def run(owner="microsoft/mssql-rs", user="Build-User", token=None, *, bypass=False,
            login_exit=0, mirror_exit=0):
        if _BASH is None:
            if os.environ.get("GHCR_TEST_REQUIRE_BASH") == "1":
                pytest.fail("Bash is required for enforced GHCR behavioral tests")
            pytest.skip("Bash is required")
        token = secrets.token_hex(24) if token is None else token
        parameters = {"ghcrOrg": owner, "ghcrUsername": user}
        variables = {
            **_PIPELINE["variables"],
            "Pipeline.Workspace": ".",
            "GHCR_TOKEN": token,
        }
        # Resolve pipeline variables as well, so an indirect parameter-to-script
        # macro regression is exercised instead of hidden by the harness.
        variables = {key: _expand(value, parameters, variables)
                     for key, value in variables.items() if isinstance(value, str)}
        enabled = bypass
        results = []
        for step in _STEPS:
            if bypass and step["displayName"] == "Check for GHCR_TOKEN":
                continue
            if "condition" in step and not enabled:
                continue
            script = step.get("script", step.get("inputs", {}).get("inlineScript", ""))
            env = {
                **os.environ,
                **{key: _expand(value, parameters, variables)
                   for key, value in step.get("env", {}).items()},
                "EXPECTED_TOKEN": token,
                "LOGIN_EXIT": str(login_exit),
                "MIRROR_EXIT": str(mirror_exit),
            }
            script_path = tmp_path / "run-step.sh"
            script_path.write_text(
                _MOCKS + _expand(script, parameters, variables),
                encoding="utf-8", newline="\n",
            )
            result = subprocess.run(
                [_BASH, script_path.name],
                cwd=tmp_path, env=env, text=True, capture_output=True,
            )
            results.append((step["displayName"], result))
            if result.returncode:
                break
            if "##vso[task.setvariable variable=GHCR_ENABLED]true" in result.stdout:
                enabled = True
        assert not (tmp_path / "injected").exists(), results
        calls_path = tmp_path / "calls"
        calls = calls_path.read_text().splitlines() if calls_path.exists() else []
        return results, calls

    return run


def test_parameters_only_enter_scripts_through_explicit_environment():
    assert "GHCR_NAMESPACE" not in _PIPELINE["variables"]
    expected = {
        "Check for GHCR_TOKEN": {"GHCR_USER_INPUT": "ghcrUsername", "GHCR_ORG_INPUT": "ghcrOrg"},
        "Login to GHCR (PAT)": {"GHCR_USER_INPUT": "ghcrUsername"},
        "Mirror images ACR -> GHCR": {"GHCR_ORG_INPUT": "ghcrOrg"},
        "GHCR summary": {"GHCR_ORG_INPUT": "ghcrOrg"},
    }
    for step in _STEPS:
        script = step.get("script", step.get("inputs", {}).get("inlineScript", ""))
        assert "${{" not in script
        assert "$(GHCR_NAMESPACE)" not in script
        assert "$(GHCR_TOKEN)" not in script
        if step["displayName"] != "Check for GHCR_TOKEN":
            assert step["condition"] == _ENABLED
        for env_name, parameter in expected.get(step["displayName"], {}).items():
            assert step["env"][env_name] == "${{ parameters." + parameter + " }}"


def test_tests_are_in_shared_validation():
    pipeline = yaml.safe_load(
        (_ROOT / ".pipeline" / "templates" / "validation-stages.yml").read_text(encoding="utf-8")
    )
    build = next(s for s in pipeline["stages"] if s["stage"] == "Build")
    windows = next(j for j in build["jobs"] if j.get("job") == "Build_Windows")
    step = next(s for s in windows["steps"] if s.get("displayName") ==
                "Unit test packaging scripts and pipeline configuration")
    assert "scripts/test_ghcr_parameters.py" in step["pwsh"].split()
    assert step.get("condition", "succeeded()") == "succeeded()"
    assert not step.get("continueOnError", False)
    linux = next(j for j in build["jobs"] if j.get("job") == "Build_Linux")
    bash_step = next(s for s in linux["steps"] if s.get("displayName") ==
                     "Test GHCR parameter handling (Linux)")
    assert "scripts/test_ghcr_parameters.py" in bash_step["bash"].split()
    assert 'python3 -m venv "$venv"' in bash_step["bash"]
    assert '"$venv/bin/python" -m pytest' in bash_step["bash"]
    assert "set -euo pipefail" in bash_step["bash"]
    assert bash_step["env"]["GHCR_TEST_REQUIRE_BASH"] == "1"
    assert bash_step.get("condition", "succeeded()") == "succeeded()"
    assert not bash_step.get("continueOnError", False)


@pytest.mark.parametrize("required", [False, True])
def test_missing_bash_cannot_silently_skip_enforced_ci(run_pipeline, monkeypatch, required):
    monkeypatch.setattr(sys.modules[__name__], "_BASH", None)
    monkeypatch.setenv("GHCR_TEST_REQUIRE_BASH", "1" if required else "0")
    expected = pytest.fail.Exception if required else pytest.skip.Exception
    with pytest.raises((pytest.fail.Exception, pytest.skip.Exception),
                       match="Bash is required") as raised:
        run_pipeline()
    assert isinstance(raised.value, expected)


@pytest.mark.parametrize("field", ["owner", "user"])
@pytest.mark.parametrize("payload", _PAYLOADS)
def test_malicious_parameters_fail_before_credentials(run_pipeline, field, payload):
    results, calls = run_pipeline(**{field: payload})
    assert len(results) == 1
    assert results[0][1].returncode == 1
    assert "ERROR: ghcr" in results[0][1].stdout
    assert not calls


@pytest.mark.parametrize("field", ["owner", "user"])
@pytest.mark.parametrize("payload", _PAYLOADS)
def test_downstream_scripts_do_not_evaluate_parameter_data(run_pipeline, field, payload):
    # Defense in depth: exercise login, mirror, and summary even without the guard.
    results, calls = run_pipeline(**{field: payload}, bypass=True)
    assert results[-1][0] == "GHCR summary"
    assert all(r.returncode == 0 for _, r in results)
    assert calls


@pytest.mark.parametrize("owner", [
    "microsoft/mssql-rs", "another-owner", "another-owner/repo.name__part--2",
    "a" * 39 + "/" + "b" * 100, "managed-user_org/repo",
])
@pytest.mark.parametrize("user", ["User-123", "a" * 39, "Managed-User_org"])
def test_valid_inputs_publish_all_images(run_pipeline, owner, user):
    results, calls = run_pipeline(owner=owner, user=user)
    assert len(results) == len(_STEPS)
    assert all(r.returncode == 0 for _, r in results)
    assert calls[:2] == [
        "az\tacr\tlogin\t--name\ttdslibrs\t",
        f"docker\tlogin\tghcr.io\t-u\t{user}\t--password-stdin\t",
    ]
    definition = next(s["script"] for s in _STEPS if s["displayName"] == "Define image list")
    images = definition.split("<<'EOF'\n", 1)[1].split("\nEOF", 1)[0].splitlines()
    assert calls[2:] == [
        f"docker\tbuildx\timagetools\tcreate\t-t\tghcr.io/{owner}/{img}\ttdslibrs.azurecr.io/{img}\t"
        for img in images
    ]
    assert f"https://github.com/orgs/{owner.split('/')[0]}/packages" in results[-1][1].stdout
    for img in images:
        assert f"ghcr.io/{owner}/{img}" in results[-1][1].stdout


@pytest.mark.parametrize("token", ["", "$(GHCR_TOKEN)"])
def test_missing_token_skips_even_invalid_parameters(run_pipeline, token):
    results, calls = run_pipeline(owner="$(touch injected)", user="", token=token)
    assert len(results) == 1
    assert results[0][1].returncode == 0
    assert "variable=GHCR_ENABLED]false" in results[0][1].stdout
    assert not calls


@pytest.mark.parametrize("owner", [
    "", "Owner", "-owner", "owner-", "two--hyphens", "a" * 40, "owner/",
    "owner/repo/nested", "owner/Repo", "owner/.repo", "owner/repo.",
    "owner/repo___name", "owner/repo..name", "owner/repo:tag",
    "owner/" + "a" * 101, "own\u00e9r", "owner/repo name", "owner_", "owner__org",
])
def test_invalid_namespace_is_rejected(run_pipeline, owner):
    results, calls = run_pipeline(owner=owner)
    assert results[0][1].returncode == 1
    assert not calls


@pytest.mark.parametrize("user", ["", "-user", "user-", "two--hyphens", "a" * 40,
                                  "user/name", "user.name", "us\u00e9r", "user name",
                                  "user_", "user__org"])
def test_invalid_username_is_rejected(run_pipeline, user):
    results, calls = run_pipeline(user=user)
    assert results[0][1].returncode == 1
    assert not calls


@pytest.mark.parametrize(("failure", "last_step"), [
    ({"login_exit": 1}, "Login to GHCR (PAT)"),
    ({"mirror_exit": 1}, "Mirror images ACR -> GHCR"),
])
def test_failure_stops_success_summary(run_pipeline, failure, last_step):
    results, _ = run_pipeline(**failure)
    assert results[-1][0] == last_step
    assert results[-1][1].returncode == 1

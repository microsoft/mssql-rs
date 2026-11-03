# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Validate the macOS deployment guard without linking an Apple artifact."""

import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest


ROOT = Path(__file__).parents[1]


@pytest.fixture(scope="module")
def build_script(tmp_path_factory):
    rustc = shutil.which("rustc")
    assert rustc, "rustc is required to test the deployment guard"
    binary = tmp_path_factory.mktemp("macos-deployment") / (
        "build-script.exe" if sys.platform == "win32" else "build-script"
    )
    subprocess.run(
        [rustc, "--edition=2024", str(ROOT / "mssql-tds" / "build.rs"), "-o", str(binary)],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )

    def run(version=None, target="x86_64-apple-darwin", os_name="macos", enabled=True):
        env = os.environ.copy()
        env.pop("MACOSX_DEPLOYMENT_TARGET", None)
        env.pop("CARGO_FEATURE_TLS_NETWORK_FRAMEWORK", None)
        env.update(RUSTC=rustc, TARGET=target, CARGO_CFG_TARGET_OS=os_name)
        if version is not None:
            env["MACOSX_DEPLOYMENT_TARGET"] = version
        if enabled:
            env["CARGO_FEATURE_TLS_NETWORK_FRAMEWORK"] = "1"
        return subprocess.run(
            [str(binary)], cwd=ROOT, env=env, capture_output=True, text=True, check=False
        )

    return run


@pytest.mark.parametrize("version", ["10.12", "10.14", "10.14.99"])
def test_network_framework_rejects_lower_targets(build_script, version):
    result = build_script(version)
    assert result.returncode != 0
    assert "requires macOS 10.15 or later" in result.stderr
    assert "MACOSX_DEPLOYMENT_TARGET=10.15" in result.stderr


@pytest.mark.parametrize("version", ["10.15", "10.15.0", "10.15.1", "11.0", "26.0"])
def test_network_framework_accepts_supported_targets(build_script, version):
    result = build_script(version)
    assert result.returncode == 0, result.stderr
    assert "rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET" in result.stdout


@pytest.mark.parametrize("version", ["", "10..15", "10.15suffix", "10.15.0.1", "-10.15"])
def test_network_framework_rejects_invalid_targets(build_script, version):
    result = build_script(version)
    assert result.returncode != 0
    assert "invalid MACOSX_DEPLOYMENT_TARGET" in result.stderr


def test_default_x86_target_is_checked_not_silently_raised(build_script):
    result = build_script()
    assert result.returncode != 0
    assert "requires macOS 10.15 or later" in result.stderr


def test_default_arm_target_is_supported(build_script):
    result = build_script(target="aarch64-apple-darwin")
    assert result.returncode == 0, result.stderr


@pytest.mark.parametrize("os_name", ["windows", "linux"])
def test_other_platforms_are_not_restricted(build_script, os_name):
    result = build_script("10.12", os_name=os_name)
    assert result.returncode == 0, result.stderr


def test_feature_disabled_retains_existing_deployment_behavior(build_script):
    result = build_script("10.12", enabled=False)
    assert result.returncode == 0, result.stderr

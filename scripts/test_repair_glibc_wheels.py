# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Unit tests for repair-glibc-wheels-in-container.sh.

Drives the shell script with a fake ``auditwheel`` (injected via ``AUDITWHEEL_BIN``)
to cover bare-glibc wheel selection, the target-platform arguments, replacement
of the original wheel, untouched musllinux/non-Linux wheels, and the two failure
modes. Everything runs with relative paths under a fixed ``cwd`` so it works the
same under WSL, Git bash, and native Linux. Skipped where bash is unavailable.

Run: pytest scripts/test_repair_glibc_wheels.py
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

_SCRIPT = Path(__file__).with_name("repair-glibc-wheels-in-container.sh")

pytestmark = pytest.mark.skipif(
    shutil.which("bash") is None, reason="bash is required to run the repair shell script"
)

_GLIBC = [
    "mssql_python_rs-0.1.0-cp310-cp310-linux_x86_64.whl",
    "mssql_python_rs-0.1.0-cp310-cp310-linux_aarch64.whl",
]
_OTHERS = [
    "mssql_python_rs-0.1.0-cp310-cp310-win_amd64.whl",
    "mssql_python_rs-0.1.0-cp310-cp310-musllinux_1_2_x86_64.whl",
    "mssql_python_rs-0.1.0-cp310-cp310-musllinux_1_2_aarch64.whl",
    "mssql_python_rs-0.1.0-cp310-cp310-macosx_15_0_universal2.whl",
]

# Fake auditwheel: log the invocation, then emit a retagged wheel whose platform
# tag is the requested --plat, matching the real tool's observable behavior.
_FAKE_PRODUCE = r"""#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "$AUDITWHEEL_LOG"
plat=""; wheeldir=""; wheel=""
while [ $# -gt 0 ]; do
  case "$1" in
    repair) shift ;;
    --plat) plat="$2"; shift 2 ;;
    --wheel-dir) wheeldir="$2"; shift 2 ;;
    --exclude) shift 2 ;;
    *) wheel="$1"; shift ;;
  esac
done
base="$(basename "$wheel")"
stem="${base%-*}"
printf 'repaired' > "$wheeldir/${stem}-${plat}.whl"
"""

# Fake auditwheel that emits a lower tag than requested (auditwheel >= 5.3 can),
# to exercise the produced-tag assertion.
_FAKE_WRONG_TAG = r"""#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "$AUDITWHEEL_LOG"
plat=""; wheeldir=""; wheel=""
while [ $# -gt 0 ]; do
  case "$1" in
    repair) shift ;;
    --plat) plat="$2"; shift 2 ;;
    --wheel-dir) wheeldir="$2"; shift 2 ;;
    --exclude) shift 2 ;;
    *) wheel="$1"; shift ;;
  esac
done
base="$(basename "$wheel")"
stem="${base%-*}"
printf 'repaired' > "$wheeldir/${stem}-${plat/2_34/2_17}.whl"
"""

# Fake auditwheel that produces no wheel, to exercise the count guard.
_FAKE_NOOP = r"""#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "$AUDITWHEEL_LOG"
"""


def _setup(tmp_path: Path, names: list[str], fake_body: str) -> None:
    shutil.copy2(_SCRIPT, tmp_path / "repair.sh")
    wheels = tmp_path / "wheels"
    wheels.mkdir()
    for name in names:
        (wheels / name).write_text(name)
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "auditwheel").write_text(fake_body, newline="\n")
    (tmp_path / "auditwheel.log").write_text("")


def _run(tmp_path: Path) -> subprocess.CompletedProcess[str]:
    # Export inside the shell: WSL does not inherit custom Windows env vars, and
    # relative paths resolve identically under WSL, Git bash, and native Linux.
    command = (
        "export AUDITWHEEL_BIN=./bin/auditwheel AUDITWHEEL_LOG=./auditwheel.log; "
        "chmod +x ./bin/auditwheel && exec bash ./repair.sh wheels"
    )
    return subprocess.run(
        ["bash", "-c", command],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=False,
    )


def test_repairs_bare_glibc_and_leaves_others(tmp_path: Path) -> None:
    _setup(tmp_path, _GLIBC + _OTHERS, _FAKE_PRODUCE)

    result = _run(tmp_path)

    assert result.returncode == 0, result.stderr
    wheels = tmp_path / "wheels"
    present = {p.name for p in wheels.glob("*.whl")}
    assert "mssql_python_rs-0.1.0-cp310-cp310-manylinux_2_34_x86_64.whl" in present
    assert "mssql_python_rs-0.1.0-cp310-cp310-manylinux_2_34_aarch64.whl" in present
    assert "mssql_python_rs-0.1.0-cp310-cp310-linux_x86_64.whl" not in present
    assert "mssql_python_rs-0.1.0-cp310-cp310-linux_aarch64.whl" not in present
    for other in _OTHERS:
        assert (wheels / other).read_text() == other
    assert "Repaired 2 glibc wheel(s)" in result.stdout
    invocations = (tmp_path / "auditwheel.log").read_text()
    assert "--plat manylinux_2_34_x86_64" in invocations
    assert "--plat manylinux_2_34_aarch64" in invocations
    assert "--exclude libssl.so.3" in invocations
    assert "--exclude libcrypto.so.3" in invocations


def test_fails_when_no_bare_glibc_wheels(tmp_path: Path) -> None:
    _setup(tmp_path, _OTHERS, _FAKE_PRODUCE)

    result = _run(tmp_path)

    assert result.returncode != 0
    assert "no bare glibc wheels" in result.stderr
    wheels = tmp_path / "wheels"
    for other in _OTHERS:
        assert (wheels / other).read_text() == other


def test_fails_when_auditwheel_produces_no_wheel(tmp_path: Path) -> None:
    _setup(tmp_path, _GLIBC, _FAKE_NOOP)

    result = _run(tmp_path)

    assert result.returncode != 0
    assert "expected exactly one repaired wheel" in result.stderr


def test_fails_when_auditwheel_emits_lower_tag(tmp_path: Path) -> None:
    _setup(tmp_path, _GLIBC, _FAKE_WRONG_TAG)

    result = _run(tmp_path)

    assert result.returncode != 0
    assert "expected platform tag" in result.stderr

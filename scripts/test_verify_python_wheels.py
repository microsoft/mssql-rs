"""Tests for final mssql-python-rs wheel validation."""

from __future__ import annotations

import subprocess
import zipfile
from pathlib import Path

_VALIDATOR = Path(__file__).parents[1] / ".pipeline" / "scripts" / "verify-python-wheels.ps1"

_LIBS = "mssql_py_core/libs"
_PLATFORM_DRIVERS = {
    "win_amd64": (f"{_LIBS}/windows/x64/mssqlodbc.dll",),
    "win_arm64": (f"{_LIBS}/windows/arm64/mssqlodbc.dll",),
    "linux_x86_64": (f"{_LIBS}/linux/glibc/x86_64/lib/mssqlodbc.so",),
    "linux_aarch64": (f"{_LIBS}/linux/glibc/arm64/lib/mssqlodbc.so",),
    "musllinux_1_2_x86_64": (f"{_LIBS}/linux/musl/x86_64/lib/mssqlodbc.so",),
    "musllinux_1_2_aarch64": (f"{_LIBS}/linux/musl/arm64/lib/mssqlodbc.so",),
    "macosx_15_0_universal2": (
        f"{_LIBS}/macos/x86_64/lib/mssqlodbc.dylib",
        f"{_LIBS}/macos/arm64/lib/mssqlodbc.dylib",
    ),
}
_PYTHON_TAGS = ("cp310", "cp311", "cp312", "cp313", "cp314")


def write_wheel(
    directory: Path,
    platform: str,
    *,
    python_tag: str = "cp313",
    distribution: str = "mssql_python_rs",
    include_odbc: bool = True,
    uppercase_odbc: bool = False,
) -> Path:
    wheel_path = directory / f"{distribution}-0.1.0-{python_tag}-{python_tag}-{platform}.whl"
    with zipfile.ZipFile(wheel_path, "w") as wheel:
        wheel.writestr(
            "mssql_python_rs-0.1.0.dist-info/METADATA",
            "Metadata-Version: 2.4\nName: mssql-python-rs\nVersion: 0.1.0\n",
        )
        wheel.writestr("mssql_py_core/__init__.py", "")
        if include_odbc:
            for driver in _PLATFORM_DRIVERS[platform]:
                if uppercase_odbc:
                    driver = driver.upper()
                wheel.writestr(driver, b"driver")
    return wheel_path


def write_wheel_matrix(directory: Path) -> list[Path]:
    wheels = [
        write_wheel(directory, platform, python_tag=python_tag)
        for python_tag in _PYTHON_TAGS
        for platform in _PLATFORM_DRIVERS
        if platform != "win_arm64"
    ]
    wheels.extend(
        write_wheel(directory, "win_arm64", python_tag=python_tag)
        for python_tag in _PYTHON_TAGS
        if python_tag != "cp310"
    )
    return wheels


def run_validator(
    wheels_dir: Path,
    *,
    require_odbc: bool = True,
) -> subprocess.CompletedProcess[str]:
    command = [
        "pwsh",
        "-NoProfile",
        "-File",
        str(_VALIDATOR),
        "-WheelsDir",
        str(wheels_dir),
        "-ExpectedName",
        "mssql-python-rs",
        "-ExpectedVersion",
        "0.1.0",
    ]
    if require_odbc:
        command.append("-RequireOdbc")
    return subprocess.run(
        command,
        capture_output=True,
        text=True,
        check=False,
    )


def test_validator_accepts_expected_wheel_matrix(tmp_path: Path) -> None:
    write_wheel_matrix(tmp_path)

    result = run_validator(tmp_path)

    assert result.returncode == 0, result.stderr
    assert "Validated 34 mssql-python-rs wheels" in result.stdout


def test_validator_rejects_missing_odbc_driver(tmp_path: Path) -> None:
    wheels = write_wheel_matrix(tmp_path)
    wheels[0].unlink()
    write_wheel(tmp_path, "win_amd64", python_tag="cp310", include_odbc=False)

    result = run_validator(tmp_path)

    assert result.returncode != 0
    assert "missing ODBC driver" in result.stderr


def test_validator_rejects_wrong_case_odbc_driver(tmp_path: Path) -> None:
    wheels = write_wheel_matrix(tmp_path)
    linux_wheel = next(wheel for wheel in wheels if "cp310-cp310-linux_x86_64" in wheel.name)
    linux_wheel.unlink()
    write_wheel(tmp_path, "linux_x86_64", python_tag="cp310", uppercase_odbc=True)

    result = run_validator(tmp_path)

    assert result.returncode != 0
    assert "missing ODBC driver" in result.stderr
    assert "mssqlodbc.so" in result.stderr


def test_validator_allows_matrix_without_odbc_when_not_required(tmp_path: Path) -> None:
    wheels = write_wheel_matrix(tmp_path)
    for wheel_path in wheels:
        platform = wheel_path.stem.rsplit("-", 1)[1]
        wheel_path.unlink()
        python_tag = wheel_path.name.split("-")[2]
        write_wheel(tmp_path, platform, python_tag=python_tag, include_odbc=False)

    result = run_validator(tmp_path, require_odbc=False)

    assert result.returncode == 0, result.stderr


def test_validator_rejects_legacy_filename(tmp_path: Path) -> None:
    wheels = write_wheel_matrix(tmp_path)
    wheels[0].unlink()
    write_wheel(tmp_path, "win_amd64", python_tag="cp310", distribution="mssql_py_core")

    result = run_validator(tmp_path)

    assert result.returncode != 0
    assert "Wheel matrix mismatch" in result.stderr
    assert "mssql_py_core-0.1.0-cp310-cp310-win_amd64.whl" in result.stderr


def test_validator_rejects_incomplete_matrix(tmp_path: Path) -> None:
    write_wheel(tmp_path, "win_amd64")

    result = run_validator(tmp_path)

    assert result.returncode != 0
    assert "Expected 34 wheels, found 1" in result.stderr


def test_validator_rejects_same_count_with_unexpected_wheel(tmp_path: Path) -> None:
    wheels = write_wheel_matrix(tmp_path)
    wheels[0].unlink()
    write_wheel(tmp_path, "win_amd64", python_tag="cp400")

    result = run_validator(tmp_path)

    assert result.returncode != 0
    assert "Wheel matrix mismatch" in result.stderr
    assert "cp310-cp310-win_amd64" in result.stderr
    assert "cp400-cp400-win_amd64" in result.stderr


def test_validator_rejects_wrong_case_wheel_filename(tmp_path: Path) -> None:
    wheels = write_wheel_matrix(tmp_path)
    linux_wheel = next(wheel for wheel in wheels if "cp310-cp310-linux_x86_64" in wheel.name)
    uppercase_name = linux_wheel.name.replace("linux", "LINUX")
    linux_wheel.rename(linux_wheel.with_name(uppercase_name))

    result = run_validator(tmp_path)

    assert result.returncode != 0
    assert "Wheel matrix mismatch" in result.stderr

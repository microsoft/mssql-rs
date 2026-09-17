#!/usr/bin/env python3
"""Install one wheel in an isolated environment and verify its native payload."""

from __future__ import annotations

import argparse
import ctypes
import importlib.metadata
import os
import platform
import re
import subprocess
import sys
import tempfile
import venv
import zipfile
from email.parser import Parser
from pathlib import Path

DIST_NAME = "mssql-python-rs"
SQL_HANDLE_ENV = 1
SQL_SUCCESS = 0


def wheel_version(wheel: Path) -> str:
    with zipfile.ZipFile(wheel) as archive:
        metadata_members = [
            name for name in archive.namelist() if name.endswith(".dist-info/METADATA")
        ]
        if len(metadata_members) != 1:
            raise RuntimeError(
                f"Expected one METADATA file in {wheel.name}, found {len(metadata_members)}"
            )
        metadata = Parser().parsestr(archive.read(metadata_members[0]).decode("utf-8"))

    if re.sub(r"[-_.]+", "-", metadata["Name"].lower()) != DIST_NAME:
        raise RuntimeError(
            f"Unexpected distribution name in {wheel.name}: {metadata['Name']}"
        )
    return metadata["Version"]


def environment_python(environment: Path) -> Path:
    if os.name == "nt":
        return environment / "Scripts" / "python.exe"
    return environment / "bin" / "python"


def expected_driver_path(wheel_name: str) -> str:
    platform_tag = Path(wheel_name).stem.rsplit("-", 1)[-1].lower()
    if platform_tag == "win_amd64":
        return "mssql_py_core/libs/windows/x64/mssqlodbc.dll"
    if platform_tag == "win_arm64":
        return "mssql_py_core/libs/windows/arm64/mssqlodbc.dll"
    if "macosx" in platform_tag and "universal2" in platform_tag:
        architecture = "arm64" if platform.machine().lower() == "arm64" else "x86_64"
        return f"mssql_py_core/libs/macos/{architecture}/lib/mssqlodbc.dylib"
    if "musllinux" in platform_tag:
        libc = "musl"
    elif "manylinux" in platform_tag:
        libc = "glibc"
    else:
        raise RuntimeError(f"Unsupported wheel platform: {platform_tag}")

    if platform_tag.endswith("_x86_64"):
        architecture = "x86_64"
    elif platform_tag.endswith("_aarch64"):
        architecture = "arm64"
    else:
        raise RuntimeError(f"Unsupported wheel architecture: {platform_tag}")
    return f"mssql_py_core/libs/linux/{libc}/{architecture}/lib/mssqlodbc.so"


def select_driver(
    distribution: importlib.metadata.Distribution, wheel_name: str
) -> Path:
    expected = expected_driver_path(wheel_name)
    files = [file for file in distribution.files or [] if str(file) == expected]

    if len(files) != 1:
        raise RuntimeError(
            f"ODBC driver not found at expected package path: {expected}"
        )
    return distribution.locate_file(files[0])


def load_driver(driver: Path):
    loader = ctypes.WinDLL if os.name == "nt" else ctypes.CDLL
    return loader(str(driver))


def verify_driver(driver: Path) -> None:
    library = load_driver(driver)
    library.SQLAllocHandle.argtypes = [
        ctypes.c_short,
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    library.SQLAllocHandle.restype = ctypes.c_short
    library.SQLFreeHandle.argtypes = [ctypes.c_short, ctypes.c_void_p]
    library.SQLFreeHandle.restype = ctypes.c_short

    environment_handle = ctypes.c_void_p()
    result = library.SQLAllocHandle(
        SQL_HANDLE_ENV, None, ctypes.byref(environment_handle)
    )
    if result != SQL_SUCCESS or not environment_handle.value:
        raise RuntimeError(f"SQLAllocHandle(SQL_HANDLE_ENV) failed with {result}")

    result = library.SQLFreeHandle(SQL_HANDLE_ENV, environment_handle)
    if result != SQL_SUCCESS:
        raise RuntimeError(f"SQLFreeHandle(SQL_HANDLE_ENV) failed with {result}")


def verify_install(expected_version: str, wheel_name: str) -> None:
    distribution = importlib.metadata.distribution(DIST_NAME)
    if distribution.version != expected_version:
        raise RuntimeError(
            f"Installed version {distribution.version}, expected {expected_version}"
        )

    import mssql_py_core

    if not Path(mssql_py_core.__file__).is_file():
        raise RuntimeError(
            "mssql_py_core did not resolve to an installed native module"
        )

    driver = select_driver(distribution, wheel_name)
    if not driver.is_file() or driver.stat().st_size == 0:
        raise RuntimeError(f"ODBC driver is missing or empty: {driver}")
    verify_driver(driver)
    print(f"Verified {DIST_NAME} {distribution.version}: {driver}")


def install_and_verify(wheel: Path) -> None:
    wheel = wheel.resolve(strict=True)
    expected_version = wheel_version(wheel)
    with tempfile.TemporaryDirectory(prefix="mssql-wheel-install-") as temp_directory:
        environment = Path(temp_directory) / "venv"
        venv.EnvBuilder(with_pip=True, clear=True).create(environment)
        python = environment_python(environment)
        subprocess.run(
            [
                str(python),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--no-index",
                "--no-deps",
                str(wheel),
            ],
            check=True,
        )
        subprocess.run(
            [
                str(python),
                str(Path(__file__).resolve()),
                "--verify",
                expected_version,
                "--wheel-name",
                wheel.name,
            ],
            check=True,
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--wheel", type=Path)
    group.add_argument("--verify", metavar="VERSION")
    parser.add_argument("--wheel-name")
    args = parser.parse_args()

    if args.verify:
        if not args.wheel_name:
            parser.error("--wheel-name is required with --verify")
        verify_install(args.verify, args.wheel_name)
    else:
        install_and_verify(args.wheel)


if __name__ == "__main__":
    main()

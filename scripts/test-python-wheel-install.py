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


def select_driver(distribution: importlib.metadata.Distribution) -> Path:
    files = distribution.files or []
    drivers = [
        distribution.locate_file(file)
        for file in files
        if file.name in {"mssqlodbc.dll", "mssqlodbc.so", "mssqlodbc.dylib"}
    ]
    if sys.platform == "darwin":
        architecture = "arm64" if platform.machine().lower() == "arm64" else "x86_64"
        drivers = [driver for driver in drivers if architecture in driver.parts]

    if len(drivers) != 1:
        raise RuntimeError(f"Expected one loadable ODBC driver, found: {drivers}")
    return drivers[0]


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


def verify_install(expected_version: str) -> None:
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

    driver = select_driver(distribution)
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
            [str(python), str(Path(__file__).resolve()), "--verify", expected_version],
            check=True,
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--wheel", type=Path)
    group.add_argument("--verify", metavar="VERSION")
    args = parser.parse_args()

    if args.verify:
        verify_install(args.verify)
    else:
        install_and_verify(args.wheel)


if __name__ == "__main__":
    main()

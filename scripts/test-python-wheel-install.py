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
import tempfile
import venv
import zipfile
from email.parser import Parser
from pathlib import Path

DIST_NAME = "mssql-python-rs"
SQL_HANDLE_ENV = 1
SQL_SUCCESS = 0
REQUIRED_ODBC_SYMBOLS = (
    "SQLAllocHandle",
    "SQLSetEnvAttr",
    "SQLSetConnectAttrW",
    "SQLSetStmtAttrW",
    "SQLGetConnectAttrW",
    "SQLDriverConnectW",
    "SQLExecDirectW",
    "SQLPrepareW",
    "SQLBindParameter",
    "SQLExecute",
    "SQLRowCount",
    "SQLGetStmtAttrW",
    "SQLSetDescFieldW",
    "SQLFetch",
    "SQLFetchScroll",
    "SQLGetData",
    "SQLNumResultCols",
    "SQLBindCol",
    "SQLDescribeColW",
    "SQLMoreResults",
    "SQLColAttributeW",
    "SQLGetTypeInfoW",
    "SQLProceduresW",
    "SQLForeignKeysW",
    "SQLPrimaryKeysW",
    "SQLSpecialColumnsW",
    "SQLStatisticsW",
    "SQLColumnsW",
    "SQLGetInfoW",
    "SQLEndTran",
    "SQLDisconnect",
    "SQLFreeHandle",
    "SQLFreeStmt",
    "SQLCancel",
    "SQLGetDiagRecW",
    "SQLParamData",
    "SQLPutData",
    "SQLTablesW",
    "SQLDescribeParam",
)


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


def verify_driver_exports(driver: Path) -> None:
    result = subprocess.run(
        ["nm", "-gU", str(driver.resolve(strict=True))],
        check=True,
        capture_output=True,
        text=True,
    )
    exports = {
        line.rsplit(maxsplit=1)[-1].removeprefix("_")
        for line in result.stdout.splitlines()
        if line.split()
    }
    missing = [name for name in REQUIRED_ODBC_SYMBOLS if name not in exports]
    if missing:
        raise RuntimeError(
            f"ODBC driver is missing required exports: {', '.join(missing)}"
        )


def verify_driver(driver: Path) -> None:
    library = load_driver(driver)
    symbols = {}
    for name in REQUIRED_ODBC_SYMBOLS:
        try:
            symbols[name] = getattr(library, name)
        except AttributeError as error:
            raise RuntimeError(
                f"ODBC driver is missing required export: {name}"
            ) from error

    allocate_handle = symbols["SQLAllocHandle"]
    free_handle = symbols["SQLFreeHandle"]
    allocate_handle.argtypes = [
        ctypes.c_short,
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    allocate_handle.restype = ctypes.c_short
    free_handle.argtypes = [ctypes.c_short, ctypes.c_void_p]
    free_handle.restype = ctypes.c_short

    environment_handle = ctypes.c_void_p()
    result = allocate_handle(SQL_HANDLE_ENV, None, ctypes.byref(environment_handle))
    if result != SQL_SUCCESS or not environment_handle.value:
        raise RuntimeError(f"SQLAllocHandle(SQL_HANDLE_ENV) failed with {result}")

    result = free_handle(SQL_HANDLE_ENV, environment_handle)
    if result != SQL_SUCCESS:
        raise RuntimeError(f"SQLFreeHandle(SQL_HANDLE_ENV) failed with {result}")


def verify_install(expected_version: str, wheel_name: str) -> None:
    distribution = importlib.metadata.distribution(DIST_NAME)
    if distribution.version != expected_version:
        raise RuntimeError(
            f"Installed version {distribution.version}, expected {expected_version}"
        )

    import mssql_py_core

    module_path = Path(mssql_py_core.__file__).resolve(strict=True)
    distribution_paths = {
        distribution.locate_file(file).resolve() for file in distribution.files or []
    }
    if module_path not in distribution_paths:
        raise RuntimeError(
            f"mssql_py_core resolved outside the installed distribution: {module_path}"
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
                "-I",
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
    group.add_argument("--verify-driver-exports", type=Path)
    parser.add_argument("--wheel-name")
    args = parser.parse_args()

    if args.verify_driver_exports:
        verify_driver_exports(args.verify_driver_exports)
    elif args.verify:
        if not args.wheel_name:
            parser.error("--wheel-name is required with --verify")
        verify_install(args.verify, args.wheel_name)
    else:
        install_and_verify(args.wheel)


if __name__ == "__main__":
    main()

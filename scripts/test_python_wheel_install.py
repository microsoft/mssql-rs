"""Tests for isolated installation of final Python wheels."""

from __future__ import annotations

import importlib.util
import zipfile
from importlib.metadata import PackagePath
from pathlib import Path

import pytest

_SCRIPT = Path(__file__).with_name("test-python-wheel-install.py")
_SPEC = importlib.util.spec_from_file_location("wheel_install", _SCRIPT)
assert _SPEC and _SPEC.loader
wheel_install = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(wheel_install)


def write_wheel(path: Path, name: str = "mssql_python_rs") -> Path:
    wheel = path / "mssql_python_rs-0.1.0-cp314-cp314-win_amd64.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(
            "mssql_python_rs-0.1.0.dist-info/METADATA",
            f"Metadata-Version: 2.4\nName: {name}\nVersion: 0.1.0\n",
        )
    return wheel


def test_wheel_version_accepts_canonicalized_distribution_name(tmp_path: Path) -> None:
    assert wheel_install.wheel_version(write_wheel(tmp_path)) == "0.1.0"


def test_wheel_version_rejects_wrong_distribution(tmp_path: Path) -> None:
    wheel = write_wheel(tmp_path, name="other-package")

    with pytest.raises(RuntimeError, match="Unexpected distribution name"):
        wheel_install.wheel_version(wheel)


def test_select_driver_uses_native_slice_for_universal2(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    class Distribution:
        files = [
            PackagePath("mssql_py_core/libs/macos/x86_64/lib/mssqlodbc.dylib"),
            PackagePath("mssql_py_core/libs/macos/arm64/lib/mssqlodbc.dylib"),
        ]

        @staticmethod
        def locate_file(file: PackagePath) -> Path:
            return tmp_path / file

    monkeypatch.setattr(wheel_install.sys, "platform", "darwin")
    monkeypatch.setattr(wheel_install.platform, "machine", lambda: "arm64")

    driver = wheel_install.select_driver(Distribution())

    assert driver.parts[-3:] == ("arm64", "lib", "mssqlodbc.dylib")


def test_verify_driver_allocates_and_frees_environment_handle(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls = []

    class Function:
        argtypes = None
        restype = None

        def __init__(self, implementation):
            self.implementation = implementation

        def __call__(self, *args):
            return self.implementation(*args)

    class Library:
        def allocate(handle_type, input_handle, output_handle):
            calls.append(("allocate", handle_type, input_handle))
            output_handle._obj.value = 42
            return wheel_install.SQL_SUCCESS

        def free(handle_type, handle):
            calls.append(("free", handle_type, handle.value))
            return wheel_install.SQL_SUCCESS

        SQLAllocHandle = Function(allocate)
        SQLFreeHandle = Function(free)

    monkeypatch.setattr(wheel_install, "load_driver", lambda _: Library())

    wheel_install.verify_driver(Path("mssqlodbc.so"))

    assert calls == [
        ("allocate", wheel_install.SQL_HANDLE_ENV, None),
        ("free", wheel_install.SQL_HANDLE_ENV, 42),
    ]


def test_verify_driver_rejects_failed_environment_allocation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class Function:
        argtypes = None
        restype = None

        def __call__(self, *_):
            return -1

    class Library:
        SQLAllocHandle = Function()
        SQLFreeHandle = Function()

    monkeypatch.setattr(wheel_install, "load_driver", lambda _: Library())

    with pytest.raises(RuntimeError, match=r"SQLAllocHandle.*failed with -1"):
        wheel_install.verify_driver(Path("mssqlodbc.so"))

"""Tests for isolated installation of final Python wheels."""

from __future__ import annotations

import importlib.util
import zipfile
from importlib.metadata import PackagePath
from pathlib import Path

import pytest

_SCRIPT = Path(__file__).with_name("test-python-wheel-install.py")
_LINUX_SCRIPT = Path(__file__).with_name("test-python-wheel-installs-linux.sh")
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


def test_linux_installs_use_mirrored_consumer_images() -> None:
    script = _LINUX_SCRIPT.read_text(encoding="utf-8")

    assert "ghcr.io/microsoft/mssql-rs/import/python-build/" in script
    assert "quay.io" not in script


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

    driver = wheel_install.select_driver(
        Distribution(),
        "mssql_python_rs-0.1.0-cp314-cp314-macosx_15_0_universal2.whl",
    )

    assert driver.parts[-3:] == ("arm64", "lib", "mssqlodbc.dylib")


@pytest.mark.parametrize(
    ("wheel_name", "expected"),
    [
        (
            "mssql_python_rs-0.1.0-cp314-cp314-win_amd64.whl",
            "mssql_py_core/libs/windows/x64/mssqlodbc.dll",
        ),
        (
            "mssql_python_rs-0.1.0-cp314-cp314-win_arm64.whl",
            "mssql_py_core/libs/windows/arm64/mssqlodbc.dll",
        ),
        (
            "mssql_python_rs-0.1.0-cp314-cp314-manylinux_2_34_x86_64.whl",
            "mssql_py_core/libs/linux/glibc/x86_64/lib/mssqlodbc.so",
        ),
        (
            "mssql_python_rs-0.1.0-cp314-cp314-manylinux_2_28_aarch64.whl",
            "mssql_py_core/libs/linux/glibc/arm64/lib/mssqlodbc.so",
        ),
        (
            "mssql_python_rs-0.1.0-cp314-cp314-musllinux_1_2_x86_64.whl",
            "mssql_py_core/libs/linux/musl/x86_64/lib/mssqlodbc.so",
        ),
        (
            "mssql_python_rs-0.1.0-cp314-cp314-musllinux_1_2_aarch64.whl",
            "mssql_py_core/libs/linux/musl/arm64/lib/mssqlodbc.so",
        ),
    ],
)
def test_expected_driver_path_matches_consumer_resolver(
    wheel_name: str, expected: str
) -> None:
    assert wheel_install.expected_driver_path(wheel_name) == expected


@pytest.mark.parametrize(
    ("wheel_name", "wrong_path"),
    [
        (
            "mssql_python_rs-0.1.0-cp314-cp314-manylinux_2_34_x86_64.whl",
            "mssql_py_core/libs/linux/glibc/arm64/lib/mssqlodbc.so",
        ),
        (
            "mssql_python_rs-0.1.0-cp314-cp314-win_amd64.whl",
            "mssql_py_core/libs/windows/arm64/mssqlodbc.dll",
        ),
    ],
)
def test_select_driver_rejects_wrong_package_path(
    tmp_path: Path, wheel_name: str, wrong_path: str
) -> None:
    class Distribution:
        files = [PackagePath(wrong_path)]

        @staticmethod
        def locate_file(file: PackagePath) -> Path:
            return tmp_path / file

    with pytest.raises(RuntimeError, match="expected package path"):
        wheel_install.select_driver(Distribution(), wheel_name)


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

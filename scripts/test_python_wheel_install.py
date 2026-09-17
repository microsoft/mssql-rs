"""Tests for isolated installation of final Python wheels."""

from __future__ import annotations

import importlib.util
import sys
import types
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


def test_linux_installs_cover_release_matrix() -> None:
    script = _LINUX_SCRIPT.read_text(encoding="utf-8")

    assert "PYTHON_TAGS=(cp310 cp311 cp312 cp313 cp314)" in script
    assert "PLATFORM_TAGS=(manylinux_2_34 manylinux_2_28 musllinux_1_2)" in script
    assert 'for platform_tag in "${PLATFORM_TAGS[@]}"' in script


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

        def __getattr__(self, name):
            assert name in wheel_install.REQUIRED_ODBC_SYMBOLS
            return Function(lambda *_: wheel_install.SQL_SUCCESS)

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

        def __getattr__(self, name):
            assert name in wheel_install.REQUIRED_ODBC_SYMBOLS
            return Function()

    monkeypatch.setattr(wheel_install, "load_driver", lambda _: Library())

    with pytest.raises(RuntimeError, match=r"SQLAllocHandle.*failed with -1"):
        wheel_install.verify_driver(Path("mssqlodbc.so"))


def test_verify_driver_rejects_missing_consumer_export(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class Function:
        argtypes = None
        restype = None

        def __call__(self, *_):
            return wheel_install.SQL_SUCCESS

    class Library:
        def __getattr__(self, name):
            if name == "SQLExecDirectW":
                raise AttributeError(name)
            return Function()

    monkeypatch.setattr(wheel_install, "load_driver", lambda _: Library())

    with pytest.raises(RuntimeError, match="missing required export: SQLExecDirectW"):
        wheel_install.verify_driver(Path("mssqlodbc.so"))


def test_verify_driver_exports_rejects_missing_consumer_export(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    driver = tmp_path / "mssqlodbc.dylib"
    driver.write_bytes(b"driver")
    calls = []
    exports = "\n".join(
        f"0000000000000000 T _{name}"
        for name in wheel_install.REQUIRED_ODBC_SYMBOLS
        if name != "SQLExecDirectW"
    )

    monkeypatch.setattr(
        wheel_install.subprocess,
        "run",
        lambda *args, **kwargs: (
            calls.append((args, kwargs)) or types.SimpleNamespace(stdout=exports)
        ),
    )

    with pytest.raises(RuntimeError, match="missing required exports: SQLExecDirectW"):
        wheel_install.verify_driver_exports(driver)
    assert calls == [
        (
            (["nm", "-gU", str(driver.resolve())],),
            {"check": True, "capture_output": True, "text": True},
        )
    ]


def test_verify_driver_exports_accepts_consumer_contract(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    driver = tmp_path / "mssqlodbc.dylib"
    driver.write_bytes(b"driver")
    exports = "\n".join(
        f"0000000000000000 T _{name}" for name in wheel_install.REQUIRED_ODBC_SYMBOLS
    )

    monkeypatch.setattr(
        wheel_install.subprocess,
        "run",
        lambda *_args, **_kwargs: types.SimpleNamespace(stdout=exports),
    )

    wheel_install.verify_driver_exports(driver)


def test_verify_install_rejects_module_outside_distribution(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    external_module = tmp_path / "external" / "mssql_py_core.py"
    external_module.parent.mkdir()
    external_module.write_text("", encoding="utf-8")
    installed_module = tmp_path / "site-packages" / "mssql_py_core.py"

    class Distribution:
        version = "0.1.0"
        files = [PackagePath("mssql_py_core.py")]

        @staticmethod
        def locate_file(file: PackagePath) -> Path:
            return installed_module.parent / file

    module = types.ModuleType("mssql_py_core")
    module.__file__ = str(external_module)
    monkeypatch.setattr(
        wheel_install.importlib.metadata, "distribution", lambda _: Distribution()
    )
    monkeypatch.setitem(sys.modules, "mssql_py_core", module)

    with pytest.raises(RuntimeError, match="outside the installed distribution"):
        wheel_install.verify_install("0.1.0", "package-0.1-cp314-cp314-win_amd64.whl")


def test_install_verification_uses_isolated_python(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    wheel = write_wheel(tmp_path)
    commands = []

    class EnvironmentBuilder:
        def create(self, _):
            pass

    monkeypatch.setattr(
        wheel_install.venv, "EnvBuilder", lambda **_: EnvironmentBuilder()
    )
    monkeypatch.setattr(
        wheel_install.subprocess,
        "run",
        lambda command, check: commands.append((command, check)),
    )

    wheel_install.install_and_verify(wheel)

    verify_command, check = commands[1]
    assert check is True
    assert verify_command[1] == "-I"
    assert verify_command[-2:] == ["--wheel-name", wheel.name]

"""Unit tests for inject-odbc-into-wheels.py.

Pure-stdlib coverage of the tag->driver-path table, wheel platform-tag
parsing, RECORD line generation, and a full inject round-trip on a synthetic
wheel. No network, no live SQL Server, no built extension required.

Run: pytest scripts/test_inject_odbc.py
"""

import importlib.util
import zipfile
from pathlib import Path

import pytest

_SCRIPT = Path(__file__).with_name("inject-odbc-into-wheels.py")
_spec = importlib.util.spec_from_file_location("inject_odbc", _SCRIPT)
inject = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(inject)


@pytest.mark.parametrize(
    "tag, expected",
    [
        ("win_amd64", ["windows/x64/mssqlodbc.dll"]),
        ("win_arm64", ["windows/arm64/mssqlodbc.dll"]),
        # glibc wheels carry the bare linux_<arch> tag (auditwheel=skip).
        ("linux_x86_64", ["linux/glibc/x86_64/lib/mssqlodbc.so"]),
        ("linux_aarch64", ["linux/glibc/arm64/lib/mssqlodbc.so"]),
        # manylinux_* is still glibc.
        ("manylinux_2_34_x86_64", ["linux/glibc/x86_64/lib/mssqlodbc.so"]),
        ("manylinux_2_28_aarch64", ["linux/glibc/arm64/lib/mssqlodbc.so"]),
        ("musllinux_1_2_x86_64", ["linux/musl/x86_64/lib/mssqlodbc.so"]),
        ("musllinux_1_2_aarch64", ["linux/musl/arm64/lib/mssqlodbc.so"]),
        (
            "macosx_15_0_universal2",
            ["macos/x86_64/lib/mssqlodbc.dylib", "macos/arm64/lib/mssqlodbc.dylib"],
        ),
    ],
)
def test_drivers_for_platform_tag(tag, expected):
    assert inject.drivers_for_platform_tag(tag) == expected


def test_musl_and_glibc_do_not_collide():
    # The whole design rests on these two resolving to different drivers.
    assert inject.drivers_for_platform_tag("musllinux_1_2_x86_64") != inject.drivers_for_platform_tag(
        "linux_x86_64"
    )


@pytest.mark.parametrize("tag", ["win_ia64", "linux_riscv64", "any", ""])
def test_unrecognized_tag_returns_empty(tag):
    assert inject.drivers_for_platform_tag(tag) == []


@pytest.mark.parametrize(
    "wheel_name, expected_tag",
    [
        ("mssql_py_core-0.1.0-cp312-cp312-win_amd64.whl", "win_amd64"),
        ("mssql_py_core-0.1.0-cp313-cp313-musllinux_1_2_aarch64.whl", "musllinux_1_2_aarch64"),
        ("mssql_py_core-0.1.0-cp311-cp311-macosx_15_0_universal2.whl", "macosx_15_0_universal2"),
    ],
)
def test_platform_tag_of(wheel_name, expected_tag):
    assert inject.platform_tag_of(wheel_name) == expected_tag


def test_record_line_format():
    line = inject._record_line("pkg/libs/x.so", b"hello")
    arc, digest, size = line.split(",")
    assert arc == "pkg/libs/x.so"
    assert digest.startswith("sha256=")
    assert "=" not in digest[len("sha256=") :]  # base64url padding stripped
    assert size == "5"


def _make_wheel(path: Path) -> None:
    with zipfile.ZipFile(path, "w") as zf:
        zf.writestr("mssql_py_core/__init__.py", b"# core\n")
        zf.writestr(
            "mssql_py_core-0.1.0.dist-info/RECORD",
            "mssql_py_core/__init__.py,,\nmssql_py_core-0.1.0.dist-info/RECORD,,\n",
        )


def test_inject_roundtrip(tmp_path):
    drivers = tmp_path / "drivers"
    (drivers / "windows" / "x64").mkdir(parents=True)
    driver_bytes = b"\x4d\x5aFAKE-DLL"
    (drivers / "windows" / "x64" / "mssqlodbc.dll").write_bytes(driver_bytes)

    wheel = tmp_path / "mssql_py_core-0.1.0-cp312-cp312-win_amd64.whl"
    _make_wheel(wheel)

    inject.inject_wheel(wheel, drivers)

    arc = "mssql_py_core/libs/windows/x64/mssqlodbc.dll"
    with zipfile.ZipFile(wheel, "r") as zf:
        names = zf.namelist()
        assert arc in names
        assert zf.read(arc) == driver_bytes
        record = zf.read("mssql_py_core-0.1.0.dist-info/RECORD").decode("utf-8")

    assert inject._record_line(arc, driver_bytes) in record.splitlines()
    # RECORD's own line stays hash-less.
    assert "mssql_py_core-0.1.0.dist-info/RECORD,," in record


def test_inject_preserves_entry_modes(tmp_path):
    drivers = tmp_path / "drivers"
    (drivers / "windows" / "x64").mkdir(parents=True)
    (drivers / "windows" / "x64" / "mssqlodbc.dll").write_bytes(b"MZ")

    wheel = tmp_path / "mssql_py_core-0.1.0-cp312-cp312-win_amd64.whl"
    with zipfile.ZipFile(wheel, "w") as zf:
        exe = zipfile.ZipInfo("mssql_py_core/_core.pyd")
        exe.external_attr = 0o755 << 16
        zf.writestr(exe, b"\x4d\x5a")
        zf.writestr(
            "mssql_py_core-0.1.0.dist-info/RECORD",
            "mssql_py_core/_core.pyd,,\nmssql_py_core-0.1.0.dist-info/RECORD,,\n",
        )

    inject.inject_wheel(wheel, drivers)

    with zipfile.ZipFile(wheel, "r") as zf:
        modes = {i.filename: (i.external_attr >> 16) & 0o777 for i in zf.infolist()}
    # Pre-existing entry keeps its stored mode (not clobbered to 0o600).
    assert modes["mssql_py_core/_core.pyd"] == 0o755
    # Injected driver gets an explicit world-readable, non-executable mode.
    assert modes["mssql_py_core/libs/windows/x64/mssqlodbc.dll"] == 0o644


def test_inject_missing_driver_fails(tmp_path):
    wheel = tmp_path / "mssql_py_core-0.1.0-cp312-cp312-win_amd64.whl"
    _make_wheel(wheel)
    with pytest.raises(SystemExit):
        inject.inject_wheel(wheel, tmp_path / "empty-drivers")


def test_inject_unknown_tag_fails(tmp_path):
    wheel = tmp_path / "mssql_py_core-0.1.0-cp312-cp312-win_ia64.whl"
    _make_wheel(wheel)
    with pytest.raises(SystemExit):
        inject.inject_wheel(wheel, tmp_path / "drivers")

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Run native key contracts against the active Python consumer installation."""

import importlib.metadata
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
TEST_FILTER = (
    "(test(connection_key::) | test(connection::tests::) | "
    "test(async_connection::tests::) | test(odbc_auth::)) "
    "& not test(emit_process_key_digest)"
)


def main():
    import mssql_python

    print(
        "Testing real mssql-python parser:",
        importlib.metadata.version("mssql-python"),
        flush=True,
    )
    env = os.environ.copy()
    env["PYO3_PYTHON"] = sys.executable
    # Standalone PyO3 test binaries otherwise initialize the base interpreter,
    # not necessarily the venv/site-packages used by the invoking Python.
    env["PYTHONPATH"] = os.pathsep.join(
        [str(Path(mssql_python.__file__).resolve().parents[1])]
        + [entry for entry in sys.path if entry]
    )
    manifest = ROOT / "mssql-py-core" / "Cargo.toml"
    if not manifest.with_name("Cargo.lock").exists():
        subprocess.run(
            ["cargo", "generate-lockfile", "--manifest-path", str(manifest)],
            cwd=ROOT,
            env=env,
            check=True,
        )
    subprocess.run(
        ["cargo", "fetch", "--locked", "--manifest-path", str(manifest)],
        cwd=ROOT,
        env=env,
        check=True,
    )
    subprocess.run(
        [
            "cargo",
            "nextest",
            "run",
            "--config-file",
            str(ROOT / ".config" / "nextest.toml"),
            "--manifest-path",
            str(manifest),
            "--frozen",
            "--lib",
            "--profile",
            "ci",
            "--run-ignored",
            "all",
            "-E",
            TEST_FILTER,
            "--no-fail-fast",
        ],
        cwd=ROOT,
        env=env,
        check=True,
    )


if __name__ == "__main__":
    main()

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""build-mssql-sqlcmd-native.sh against a stub cargo: the archive is staged from
the target directory cargo reports, not from an assumed <repo>/target.

Runs under `python3 -m unittest` in the Linux validation job; the Windows script
is covered by scripts/test_release_pipeline.py on the Windows job."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
TARGET = "x86_64-unknown-linux-gnu"

# Stands in for cargo. metadata prints FAKE_TARGET_DIR as target_directory,
# compact like cargo or pretty-printed (FAKE_METADATA_PRETTY), or omits it
# (FAKE_METADATA_NO_TARGET); rustc writes a "fresh" archive there.
FAKE_CARGO = """\
import json, os, sys
args = sys.argv[1:]
target_dir = os.environ["FAKE_TARGET_DIR"]
if args and args[0] == "metadata":
    doc = {"packages": [{"name": "mssql-sqlcmd", "version": "0.1.0"}]}
    if not os.environ.get("FAKE_METADATA_NO_TARGET"):
        doc["target_directory"] = target_dir
    if os.environ.get("FAKE_METADATA_PRETTY"):
        print(json.dumps(doc, indent=2))
    else:
        print(json.dumps(doc, separators=(",", ":")))
elif args and args[0] == "rustc":
    target = args[args.index("--target") + 1]
    release = os.path.join(target_dir, target, "release")
    os.makedirs(release, exist_ok=True)
    with open(os.path.join(release, "libmssql_sqlcmd.a"), "w") as archive:
        archive.write("fresh\\n")
    print("note: native-static-libs: -lfake", file=sys.stderr)
"""


@unittest.skipIf(sys.platform == "win32" or shutil.which("bash") is None, "needs a Unix bash")
class StagesFromCargosTargetDirectory(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)

        repo = self.root / "repo"
        scripts = repo / ".pipeline" / "scripts"
        scripts.mkdir(parents=True)
        self.script = scripts / "build-mssql-sqlcmd-native.sh"
        shutil.copy2(HERE / self.script.name, self.script)

        # A stale archive where the old script looked: <repo>/target.
        stale = repo / "target" / TARGET / "release"
        stale.mkdir(parents=True)
        (stale / "libmssql_sqlcmd.a").write_text("stale\n", encoding="utf-8")

        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        (bin_dir / "fake_cargo.py").write_text(FAKE_CARGO, encoding="utf-8")
        for tool, run in (
            ("cargo", f'exec "{sys.executable}" "$(dirname "$0")/fake_cargo.py" "$@"'),
            ("rustup", "exit 0"),
        ):
            stub = bin_dir / tool
            stub.write_text(f"#!/usr/bin/env bash\n{run}\n", encoding="utf-8", newline="\n")
            stub.chmod(0o755)

        self.env = {k: v for k, v in os.environ.items() if k != "CARGO_TARGET_DIR"}
        self.env["PATH"] = str(bin_dir) + os.pathsep + self.env["PATH"]
        self.env["FAKE_TARGET_DIR"] = str(self.root / "configured-target")
        self.out = self.root / "out"

    def build(self, **env):
        return subprocess.run(
            ["bash", str(self.script), TARGET, "linux-x64", str(self.out)],
            env={**self.env, **env},
            capture_output=True,
            text=True,
            check=False,
        )

    def assert_staged_fresh(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        native = self.out / "runtimes" / "linux-x64" / "native"
        self.assertEqual((native / "libmssql_sqlcmd.a").read_text(encoding="utf-8"), "fresh\n")
        self.assertEqual((native / "native-static-libs.txt").read_text(encoding="ascii").strip(), "-lfake")

    def test_stages_the_archive_from_cargos_target_directory(self):
        self.assert_staged_fresh(self.build())

    def test_reads_pretty_printed_metadata(self):
        self.assert_staged_fresh(self.build(FAKE_METADATA_PRETTY="1"))

    def test_fails_when_cargo_reports_no_target_directory(self):
        result = self.build(FAKE_METADATA_NO_TARGET="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cargo metadata reports no target_directory", result.stderr)
        self.assertFalse((self.out / "runtimes").exists())


if __name__ == "__main__":
    unittest.main()

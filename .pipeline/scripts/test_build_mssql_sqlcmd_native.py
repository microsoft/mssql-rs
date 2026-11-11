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
    libs = os.environ.get("FAKE_NATIVE_LIBS", "-lfake")
    print(f"note: native-static-libs: {libs}", file=sys.stderr)
"""

# Stand in for the link check's tools. cc records the libraries it links and
# fails with FAKE_CC_FAIL; nm fails with FAKE_NM_FAIL or lists no API with
# FAKE_NM_EMPTY; readelf reports FAKE_NEEDED as a needed library and fails with
# FAKE_READELF_FAIL.
FAKE_CC = """\
#!/usr/bin/env bash
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
printf '%s\\n' "$*" > "$FAKE_CC_LOG"
[ -n "${FAKE_CC_FAIL:-}" ] && { echo "undefined reference to \\`SSL_new'" >&2; exit 1; }
: > "$out"
"""
FAKE_NM = """\
#!/usr/bin/env bash
[ -n "${FAKE_NM_FAIL:-}" ] && { echo "nm: file format not recognized" >&2; exit 1; }
[ -n "${FAKE_NM_EMPTY:-}" ] && { echo "0000000000000000 T rust_helper"; exit 0; }
echo "0000000000000000 T mssql_sqlcmd_free_text"
"""
FAKE_READELF = """\
#!/usr/bin/env bash
[ -n "${FAKE_READELF_FAIL:-}" ] && { echo "readelf: Error: Not an ELF file" >&2; exit 1; }
[ -n "${FAKE_NEEDED:-}" ] && echo " 0x0000000000000001 (NEEDED) Shared library: [$FAKE_NEEDED]"
exit 0
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
        for tool, text in (("cc", FAKE_CC), ("nm", FAKE_NM), ("readelf", FAKE_READELF)):
            stub = bin_dir / tool
            stub.write_text(text, encoding="utf-8", newline="\n")
            stub.chmod(0o755)

        self.env = {k: v for k, v in os.environ.items() if k not in ("CARGO_TARGET_DIR", "CC")}
        self.env["PATH"] = str(bin_dir) + os.pathsep + self.env["PATH"]
        self.env["FAKE_TARGET_DIR"] = str(self.root / "configured-target")
        self.env["FAKE_CC_LOG"] = str(self.root / "cc.log")
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
        # glibc before 2.34 keeps dlopen in libdl, which openssl_runtime uses.
        self.assertEqual((native / "native-static-libs.txt").read_text(encoding="ascii").strip(), "-lfake -ldl")

    def test_stages_the_archive_from_cargos_target_directory(self):
        self.assert_staged_fresh(self.build())

    def test_reads_pretty_printed_metadata(self):
        self.assert_staged_fresh(self.build(FAKE_METADATA_PRETTY="1"))

    def test_fails_when_cargo_reports_no_target_directory(self):
        result = self.build(FAKE_METADATA_NO_TARGET="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cargo metadata reports no target_directory", result.stderr)
        self.assertFalse((self.out / "runtimes").exists())

    def test_drops_openssl_from_the_linux_libraries(self):
        # The archive loads OpenSSL at run time; consumers must not link it.
        result = self.build(FAKE_NATIVE_LIBS="-lssl -lcrypto -lgcc_s -lpthread -ldl -lc")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        native = self.out / "runtimes" / "linux-x64" / "native"
        staged = (native / "native-static-libs.txt").read_text(encoding="ascii").strip()
        self.assertEqual(staged, "-lgcc_s -lpthread -ldl -lc")
        # The link check links the staged archive with exactly that list.
        linked = (self.root / "cc.log").read_text(encoding="utf-8")
        self.assertIn(str(native / "libmssql_sqlcmd.a"), linked)
        self.assertTrue(linked.rstrip().endswith("-lgcc_s -lpthread -ldl -lc"), linked)
        self.assertNotIn("-lssl", linked)

    def test_fails_when_the_archive_does_not_link_without_openssl(self):
        result = self.build(FAKE_CC_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not link with only its native-static-libs", result.stderr)
        self.assertIn("openssl_runtime.rs", result.stderr)

    def test_fails_when_the_linked_program_still_needs_openssl(self):
        result = self.build(FAKE_NEEDED="libssl.so.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("links OpenSSL; it must load it at run time", result.stderr)

    def test_fails_when_nm_cannot_read_the_archive(self):
        result = self.build(FAKE_NM_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nm could not read the symbols", result.stderr)
        self.assertFalse((self.root / "cc.log").exists())

    def test_fails_when_the_archive_defines_no_api(self):
        result = self.build(FAKE_NM_EMPTY="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("defines no mssql_sqlcmd_* functions", result.stderr)
        self.assertFalse((self.root / "cc.log").exists())

    def test_fails_when_readelf_cannot_inspect_the_link_check(self):
        result = self.build(FAKE_READELF_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("readelf could not read the dependencies", result.stderr)
        self.assertNotIn("Staged linux-x64", result.stdout)


if __name__ == "__main__":
    unittest.main()

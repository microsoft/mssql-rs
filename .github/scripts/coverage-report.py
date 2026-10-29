# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Process untrusted coverage without loading code or reports from the PR tree."""

import argparse
import json
import math
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
import zipfile


def extract_coverage(archive_path: Path, destination: Path) -> None:
    expected = "CoberturaCoverage/cobertura.xml"
    with zipfile.ZipFile(archive_path) as archive:
        files = []
        for member in archive.infolist():
            name = member.orig_filename
            kind = stat.S_IFMT(member.external_attr >> 16)
            if name == "CoberturaCoverage/" and member.is_dir():
                if kind not in (0, stat.S_IFDIR):
                    raise ValueError("Coverage archive directory is not a directory")
                continue
            if name != expected or kind not in (0, stat.S_IFREG):
                raise ValueError(f"Unexpected coverage archive member: {name!r}")
            files.append(member)
        if len(files) != 1:
            raise ValueError("Coverage archive must contain exactly one regular cobertura.xml")
        # Copy only the allowlisted member; never interpret archive paths on disk.
        with archive.open(files[0]) as source, destination.open("xb") as target:
            shutil.copyfileobj(source, target)


def percentage(value: object, maximum: float = 100) -> float:
    if type(value) not in (int, float) or not math.isfinite(value) or not 0 <= value <= maximum:
        raise ValueError(f"Invalid coverage value: {value!r}")
    return float(value)


def generate_report(archive: Path, source: Path, build_id: str, temp_dir: Path) -> dict:
    if not re.fullmatch(r"[1-9][0-9]*", build_id):
        raise ValueError("Invalid Azure DevOps build ID")
    with tempfile.TemporaryDirectory(prefix="coverage-report-", dir=temp_dir) as directory:
        work = Path(directory)
        xml = work / "coverage.xml"
        extract_coverage(archive, xml)
        root = ET.parse(xml).getroot()
        if root.tag != "coverage":
            raise ValueError("Expected a Cobertura coverage root")
        rate = root.get("line-rate")
        overall = "N/A" if rate is None else f"{percentage(float(rate), 1) * 100:.1f}%"
        del root
        report_json = work / "patch-coverage.json"
        report_md = work / "patch-coverage.md"
        subprocess.run(
            [
                sys.executable, "-I", "-m", "diff_cover.diff_cover_tool", str(xml),
                "--compare-branch=origin/main",
                "--ignore-staged", "--ignore-unstaged",
                "--format", f"json:{report_json},markdown:{report_md}",
            ],
            cwd=source,
            check=True,
        )
        data = json.loads(report_json.read_text(encoding="utf-8"))
        patch = percentage(data["total_percent_covered"])
        summary = report_md.read_text(encoding="utf-8")
        if len(summary) > 50000:
            summary = summary[:50000] + "\n\n_Report truncated to fit a PR comment._"
        return {
            "coverage_percentage": overall,
            "patch_coverage_pct": f"{patch:g}%",
            "patch_coverage_summary": summary,
            "ado_url": f"https://dev.azure.com/sqlclientdrivers/public/_build/results?buildId={build_id}",
        }


def write_output(output: Path, report: dict) -> None:
    # JSON escapes newlines, including any attacker-supplied delimiter or assignment.
    encoded = json.dumps(report, ensure_ascii=True, allow_nan=False)
    with output.open("a", encoding="utf-8", newline="\n") as stream:
        stream.write(f"report={encoded}\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--build-id", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--temp-dir", type=Path, required=True)
    args = parser.parse_args()
    report = generate_report(
        args.archive.resolve(), args.source.resolve(), args.build_id, args.temp_dir.resolve()
    )
    write_output(args.output, report)


if __name__ == "__main__":
    main()

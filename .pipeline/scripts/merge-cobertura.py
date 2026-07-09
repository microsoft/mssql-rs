#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Merge multiple Cobertura coverage reports into a single report.

Azure DevOps does not merge coverage produced by different pipeline stages or
jobs, so the mssql-tds Rust coverage (Windows) and the mssql-py-core coverage
(Python tests on Linux) are combined here into one Cobertura file that the ADO
coverage tab and the GitHub diff-cover workflow can consume.

Merge semantics: line hits are unioned across inputs (a line covered by either
report is covered in the result), keyed by (filename, line number). This is the
correct semantic for combining coverage from independent test runs and safely
handles files that appear in more than one input (for example mssql-tds, which
both the Rust suite and the Python bindings exercise).

Usage:
    merge-cobertura.py --output OUT.xml INPUT[::PREFIX] [INPUT[::PREFIX] ...]

INPUT is a path to a Cobertura XML file. Missing inputs are skipped with a
warning so the merge stays resilient when an upstream job did not publish
coverage. An optional ``::PREFIX`` prepends PREFIX to every relative filename in
that input before normalization, which is used to make paths from a crate that
was measured in its own directory (mssql-py-core) repo-root relative.
"""

from __future__ import annotations

import argparse
import sys
import xml.etree.ElementTree as ET
from xml.dom import minidom


def normalize_path(filename: str, prefix: str) -> str:
    """Return a repo-root-relative, forward-slash path for a source file."""
    fn = filename.replace("\\", "/")
    if prefix and not fn.startswith("/") and len(fn) > 1 and fn[1] != ":":
        fn = f"{prefix.rstrip('/')}/{fn}"

    parts: list[str] = []
    for part in fn.split("/"):
        if part in ("", "."):
            continue
        if part == "..":
            if parts:
                parts.pop()
            continue
        parts.append(part)

    # Absolute agent paths look like /workspace/<repo>/... - keep only the
    # portion after the last "workspace" segment so paths become repo relative.
    if "workspace" in parts:
        idx = len(parts) - 1 - parts[::-1].index("workspace")
        parts = parts[idx + 1:]

    return "/".join(parts)


class LineData:
    __slots__ = ("hits", "branch", "conditions")

    def __init__(self, hits: int, branch: bool, conditions: str | None):
        self.hits = hits
        self.branch = branch
        self.conditions = conditions

    def merge(self, hits: int, branch: bool, conditions: str | None) -> None:
        # Union of hits: covered by either run counts as covered.
        if hits > self.hits:
            self.hits = hits
            # Prefer condition data from the run that actually exercised the line.
            if conditions:
                self.conditions = conditions
        if branch:
            self.branch = True
            if conditions and not self.conditions:
                self.conditions = conditions


class ClassData:
    def __init__(self, name: str, filename: str):
        self.name = name
        self.filename = filename
        self.lines: dict[int, LineData] = {}


def parse_input(path: str, prefix: str, classes: dict[str, ClassData]) -> bool:
    try:
        tree = ET.parse(path)
    except FileNotFoundError:
        print(f"warning: coverage input not found, skipping: {path}", file=sys.stderr)
        return False
    except ET.ParseError as exc:
        print(f"warning: could not parse {path}: {exc}", file=sys.stderr)
        return False

    root = tree.getroot()
    for cls in root.iter("class"):
        filename = cls.get("filename", "")
        if not filename:
            continue
        norm = normalize_path(filename, prefix)
        entry = classes.get(norm)
        if entry is None:
            entry = ClassData(cls.get("name", norm), norm)
            classes[norm] = entry

        for line in cls.iter("line"):
            try:
                number = int(line.get("number", "0"))
            except ValueError:
                continue
            try:
                hits = int(line.get("hits", "0"))
            except ValueError:
                hits = 0
            branch = line.get("branch", "false") == "true"
            conditions = line.get("condition-coverage")

            existing = entry.lines.get(number)
            if existing is None:
                entry.lines[number] = LineData(hits, branch, conditions)
            else:
                existing.merge(hits, branch, conditions)

    return True


def package_name(filename: str) -> str:
    """Derive a package name (top-level directory) for grouping classes."""
    head = filename.split("/", 1)[0]
    return head or "."


def rate(covered: int, total: int) -> float:
    return (covered / total) if total else 0.0


def build_document(classes: dict[str, ClassData]) -> ET.Element:
    packages: dict[str, list[ClassData]] = {}
    for cls in classes.values():
        packages.setdefault(package_name(cls.filename), []).append(cls)

    total_lines = 0
    covered_lines = 0

    coverage = ET.Element("coverage")
    sources = ET.SubElement(coverage, "sources")
    ET.SubElement(sources, "source").text = "."
    packages_el = ET.SubElement(coverage, "packages")

    for pkg_name in sorted(packages):
        pkg_classes = packages[pkg_name]
        pkg_total = 0
        pkg_covered = 0
        package_el = ET.SubElement(packages_el, "package", {"name": pkg_name})
        classes_el = ET.SubElement(package_el, "classes")

        for cls in sorted(pkg_classes, key=lambda c: c.filename):
            cls_total = len(cls.lines)
            cls_covered = sum(1 for ln in cls.lines.values() if ln.hits > 0)
            pkg_total += cls_total
            pkg_covered += cls_covered

            class_el = ET.SubElement(
                classes_el,
                "class",
                {
                    "name": cls.name,
                    "filename": cls.filename,
                    "line-rate": f"{rate(cls_covered, cls_total):.4f}",
                    "branch-rate": "0",
                    "complexity": "0",
                },
            )
            ET.SubElement(class_el, "methods")
            lines_el = ET.SubElement(class_el, "lines")
            for number in sorted(cls.lines):
                ln = cls.lines[number]
                attrs = {
                    "number": str(number),
                    "hits": str(ln.hits),
                    "branch": "true" if ln.branch else "false",
                }
                if ln.conditions:
                    attrs["condition-coverage"] = ln.conditions
                ET.SubElement(lines_el, "line", attrs)

        package_el.set("line-rate", f"{rate(pkg_covered, pkg_total):.4f}")
        package_el.set("branch-rate", "0")
        package_el.set("complexity", "0")
        total_lines += pkg_total
        covered_lines += pkg_covered

    coverage.set("line-rate", f"{rate(covered_lines, total_lines):.4f}")
    coverage.set("branch-rate", "0")
    coverage.set("lines-covered", str(covered_lines))
    coverage.set("lines-valid", str(total_lines))
    coverage.set("branches-covered", "0")
    coverage.set("branches-valid", "0")
    coverage.set("complexity", "0")
    coverage.set("version", "0")
    coverage.set("timestamp", "0")
    return coverage


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, help="Path to the merged Cobertura XML.")
    parser.add_argument(
        "inputs",
        nargs="+",
        help="Input Cobertura files, each optionally suffixed with ::PREFIX.",
    )
    args = parser.parse_args(argv)

    classes: dict[str, ClassData] = {}
    merged_any = False
    for item in args.inputs:
        if "::" in item:
            path, prefix = item.split("::", 1)
        else:
            path, prefix = item, ""
        if parse_input(path, prefix, classes):
            merged_any = True
            print(f"merged coverage input: {path}" + (f" (prefix {prefix})" if prefix else ""))

    if not merged_any:
        print("error: no coverage inputs could be read", file=sys.stderr)
        return 1

    document = build_document(classes)
    xml_bytes = ET.tostring(document, encoding="utf-8")
    pretty = minidom.parseString(xml_bytes).toprettyxml(indent="  ", encoding="utf-8")
    with open(args.output, "wb") as handle:
        handle.write(pretty)

    covered = int(document.get("lines-covered", "0"))
    valid = int(document.get("lines-valid", "0"))
    pct = (covered / valid * 100) if valid else 0.0
    print(f"wrote {args.output}: {covered}/{valid} lines covered ({pct:.1f}%)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

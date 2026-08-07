# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Parse the two ctest JUnit XMLs from a --compare-with-msodbcsql run and print a
# parity table. Exits non-zero when any test reached a different verdict in the
# two legs, so run_e2e.sh fails the run on a divergence or shared failure.
#
# Usage: parity_report.py <mssql-odbc-junit.xml> <msodbcsql-junit.xml>

import sys
import xml.etree.ElementTree as ET


def load(path):
    """Returns {test_name: 'PASS'|'FAIL'|'SKIP'}."""
    out = {}
    try:
        root = ET.parse(path).getroot()
    except (ET.ParseError, FileNotFoundError):
        return out
    # ctest --output-junit emits <testsuite><testcase name="..."> ...
    for tc in root.iter("testcase"):
        name = tc.get("name") or "<unnamed>"
        tags = {child.tag for child in tc}
        if tags & {"failure", "error"}:
            out[name] = "FAIL"
        elif "skipped" in tags:
            out[name] = "SKIP"
        else:
            out[name] = "PASS"
    return out


# Verdicts describe only the observed outcome pairing, not a root cause: a
# per-test PASS/FAIL divergence does not by itself establish which side is
# wrong, and a shared failure does not prove the test is buggy.
def verdict(r, m):
    if r == "SKIP" or m == "SKIP":  return ("skipped (not compared)", "skip")
    if r == "PASS" and m == "PASS": return ("parity", "parity")
    if r == "FAIL" and m == "FAIL": return ("shared failure - investigate", "shared")
    if r != m:                      return ("divergence - investigate", "divergence")
    return ("missing run - investigate", "divergence")


def main(argv):
    if len(argv) != 3:
        print("usage: parity_report.py <mssql-odbc-junit.xml> <msodbcsql-junit.xml>",
              file=sys.stderr)
        return 2

    rust = load(argv[1])
    ms = load(argv[2])
    names = sorted(set(rust) | set(ms))

    w = max((len(n) for n in names), default=4)
    print()
    print("=== Parity report (mssql-odbc vs msodbcsql) ===")
    print(f"{'Test'.ljust(w)}  {'mssql-odbc':<10}  {'msodbcsql':<10}  Verdict")
    print(f"{'-'*w}  {'-'*10}  {'-'*10}  {'-'*30}")
    counts = {"parity": 0, "divergence": 0, "shared": 0, "skip": 0}
    for n in names:
        r = rust.get(n, "MISSING")
        m = ms.get(n, "MISSING")
        v, kind = verdict(r, m)
        counts[kind] += 1
        print(f"{n.ljust(w)}  {r:<10}  {m:<10}  {v}")
    print()
    print(f"Summary: {counts['parity']} parity, {counts['divergence']} divergence(s), "
          f"{counts['shared']} shared failure(s), {counts['skip']} skipped")

    # Any non-parity outcome fails the run. ctest exit codes alone are not enough:
    # a test present in only one leg leaves both legs green while the comparison is
    # meaningless.
    return 1 if counts["divergence"] or counts["shared"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

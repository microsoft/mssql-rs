# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Offline tests for the ODBC benchmark comparator's policy and reporting.

The comparator decides whether a perf run passes, so the parts worth testing are
the ones that can be wrong without anything crashing: a benchmark silently
dropped from one leg, a confirmed regression that fails to gate, an unconfirmed
one that gates anyway, and a report that omits a whole workload family.

Everything here is synthetic - no SQL Server, no ODBC driver, no Google Benchmark
build. Google Benchmark's own ``tools/compare.py`` is exercised only when a
checkout and NumPy/SciPy happen to be present; the default path uses the
comparator's built-in medians so the suite runs anywhere.

Run with ``python -m unittest discover .pipeline/scripts``.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
COMPARATOR = REPO_ROOT / ".pipeline" / "scripts" / "compare-odbc-benchmarks.py"

# The C++ workload catalog. Written out rather than imported so a rename in the
# harness shows up here as a failing expectation instead of being silently
# mirrored.
BENCHMARKS = (
    "fetch/narrow_2m_c15_fixed/bound_rowset_1000",
    "fetch/wide_10k_c600_fixed/bound_rowset_1000",
    "fetch/rowset_100k_c15_fixed/bound_rowset_1",
    "fetch/rowset_100k_c15_fixed/bound_rowset_64",
    "fetch/rowset_100k_c15_fixed/bound_rowset_1000",
    "fetch/rowset_100k_c15_fixed/bind_cycle_rowset_64",
    "fetch/rowset_20k_c15_fixed/bind_cycle_rowset_1",
    "fetch/varwidth_100k_c7_nullable/bound_rowset_1000",
    "fetch/varwidth_100k_c7_nullable/bound_rowset_64",
    "getdata/rowwise_20k_c15_fixed/inline_values",
    "getdata/rowwise_1k_c3_lob_max/chunked_8192",
    "getdata/rowwise_20k_c16_mixed_lob/whole_result_rowwise",
    "getdata/rowwise_20k_c4_variant/probe_colattribute",
)

GETDATA_BENCHMARKS = tuple(
    name for name in BENCHMARKS if name.startswith("getdata/")
)

NAME_SUFFIX = "/iterations:1/manual_time"
REPETITIONS = 5


def run_name(benchmark_id):
    """Google Benchmark's manual-timing run name, which is the comparison key."""
    return benchmark_id + NAME_SUFFIX


def make_document(benchmark_ids, milliseconds, rows=1000):
    """Build a Google Benchmark document with the shape both harnesses emit.

    ``milliseconds`` maps a benchmark id to its per-repetition time; a scalar
    applies to every id. Aggregates are included because a real run has them and
    the comparator prefers them.
    """
    benchmarks = []
    for family_index, benchmark_id in enumerate(benchmark_ids):
        name = run_name(benchmark_id)
        value = (
            milliseconds[benchmark_id]
            if isinstance(milliseconds, dict)
            else milliseconds
        )
        for repetition_index in range(REPETITIONS):
            benchmarks.append(
                {
                    "name": name,
                    "family_index": family_index,
                    "per_family_instance_index": 0,
                    "run_name": name,
                    "run_type": "iteration",
                    "repetitions": REPETITIONS,
                    "repetition_index": repetition_index,
                    "threads": 1,
                    "iterations": 1,
                    "real_time": value,
                    "cpu_time": value,
                    "time_unit": "ms",
                    "rows": float(rows),
                    "cells": float(rows * 15),
                    "rows_per_second": rows / (value / 1000.0),
                    "cells_per_second": rows * 15 / (value / 1000.0),
                    "logical_bytes_per_second": rows * 100 / (value / 1000.0),
                }
            )
        benchmarks.append(
            {
                "name": f"{name}_median",
                "family_index": family_index,
                "per_family_instance_index": 0,
                "run_name": name,
                "run_type": "aggregate",
                "repetitions": REPETITIONS,
                "threads": 1,
                "aggregate_name": "median",
                "aggregate_unit": "time",
                "iterations": REPETITIONS,
                "real_time": value,
                "cpu_time": value,
                "time_unit": "ms",
            }
        )
    return {"context": {"host_name": "synthetic"}, "benchmarks": benchmarks}


class ComparatorTestCase(unittest.TestCase):
    """Shared plumbing: write synthetic legs and invoke the comparator."""

    def setUp(self):
        if not COMPARATOR.is_file():
            self.skipTest(f"comparator not found at {COMPARATOR}")
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(self._temp.name)
        self.addCleanup(self._temp.cleanup)

    def write(self, name, document):
        path = self.root / name
        path.write_text(json.dumps(document), encoding="utf-8")
        return path

    def compare(self, arguments, output_dir=None):
        """Run the comparator and return (exit code, stdout+stderr, output dir)."""
        output_dir = output_dir or (self.root / "out")
        completed = subprocess.run(
            [sys.executable, str(COMPARATOR), *arguments, "--output-dir", str(output_dir)],
            capture_output=True,
            text=True,
            check=False,
        )
        return completed.returncode, completed.stdout + completed.stderr, output_dir

    def three_way(self, baseline_ms, candidate_ms, reference_ms, ids=BENCHMARKS):
        """Write one full three-driver pass and return its comparator arguments."""
        baseline = self.write("baseline.json", make_document(ids, baseline_ms))
        candidate = self.write("candidate.json", make_document(ids, candidate_ms))
        reference = self.write("reference.json", make_document(ids, reference_ms))
        return [
            "--baseline",
            str(baseline),
            "--candidate",
            str(candidate),
            "--reference",
            str(reference),
            "--reference-label",
            "Microsoft ODBC 18.6.2.1",
            "--reference-version",
            "18.6.2.1",
            "--baseline-commit",
            "0" * 40,
            "--repetitions",
            str(REPETITIONS),
        ]


class ThreeWayReportTests(ComparatorTestCase):
    """The whole workload catalog must reach the report and the gate."""

    def test_clean_run_passes_and_reports_every_benchmark(self):
        code, _, out = self.compare(self.three_way(100.0, 100.0, 40.0))
        self.assertEqual(code, 0)
        summary = (out / "summary.md").read_text(encoding="utf-8")
        for benchmark_id in BENCHMARKS:
            self.assertIn(f"`{run_name(benchmark_id)}`", summary)
        report = json.loads((out / "comparison.json").read_text(encoding="utf-8"))
        self.assertEqual(len(report["benchmarks"]), len(BENCHMARKS))

    def test_getdata_rows_carry_throughput_and_the_reference_column(self):
        code, _, out = self.compare(self.three_way(100.0, 100.0, 40.0))
        self.assertEqual(code, 0)
        report = json.loads((out / "comparison.json").read_text(encoding="utf-8"))
        getdata_rows = [
            row
            for row in report["benchmarks"]
            if row["name"].startswith("getdata/")
        ]
        self.assertEqual(len(getdata_rows), len(GETDATA_BENCHMARKS))
        for row in getdata_rows:
            self.assertGreater(row["rows_per_second"], 0)
            self.assertGreater(row["cells_per_second"], 0)
            self.assertIn("candidate_vs_reference_ratio", row)
            self.assertAlmostEqual(row["candidate_vs_reference_ratio"], 2.5, places=6)

    def test_microsoft_gap_is_reported_but_never_gates(self):
        # Candidate is 5x the Microsoft reference and identical to the baseline:
        # informational, so the run still passes.
        code, _, out = self.compare(self.three_way(100.0, 100.0, 20.0))
        self.assertEqual(code, 0)
        summary = (out / "summary.md").read_text(encoding="utf-8")
        self.assertIn("Informational means it does not fail this regression run", summary)
        self.assertIn("✅", summary)


class GateAndConfirmationTests(ComparatorTestCase):
    """The gate is the whole point: it has to fire on a confirmed regression and
    stay quiet on one that did not reproduce."""

    def regressed_candidate(self, benchmark_id, ratio=1.30):
        candidate = {name: 100.0 for name in BENCHMARKS}
        candidate[benchmark_id] = 100.0 * ratio
        return candidate

    def test_initial_regression_without_confirmation_data_is_rejected(self):
        target = BENCHMARKS[0]
        arguments = self.three_way(100.0, self.regressed_candidate(target), 40.0)
        code, output, _ = self.compare(arguments + ["--confirm-runs", "4"])
        self.assertEqual(code, 1)
        self.assertIn("confirmation results are missing", output)

    def test_confirmed_regression_fails_the_run(self):
        target = BENCHMARKS[2]
        arguments = self.three_way(100.0, self.regressed_candidate(target), 40.0)
        code, _, out = self.compare(
            arguments
            + [
                "--confirm-runs",
                "4",
                "--confirm-quorum",
                "3",
                "--confirmation",
                run_name(target),
                "4",
                "4",
                "1.28",
            ]
        )
        self.assertEqual(code, 2)
        summary = (out / "summary.md").read_text(encoding="utf-8")
        self.assertIn("❌", summary)
        self.assertIn("confirmed regression", summary)

    def test_unconfirmed_regression_clears_the_gate(self):
        target = BENCHMARKS[2]
        arguments = self.three_way(100.0, self.regressed_candidate(target), 40.0)
        code, _, out = self.compare(
            arguments
            + [
                "--confirm-runs",
                "4",
                "--confirm-quorum",
                "3",
                "--confirmation",
                run_name(target),
                "1",
                "1",
                "1.01",
            ]
        )
        self.assertEqual(code, 0)
        summary = (out / "summary.md").read_text(encoding="utf-8")
        self.assertIn("✅", summary)
        self.assertIn("not confirmed", summary)

    def test_a_getdata_regression_gates_exactly_like_a_bound_fetch_one(self):
        target = GETDATA_BENCHMARKS[0]
        arguments = self.three_way(100.0, self.regressed_candidate(target), 40.0)
        code, _, out = self.compare(
            arguments
            + [
                "--confirm-runs",
                "4",
                "--confirm-quorum",
                "3",
                "--confirmation",
                run_name(target),
                "3",
                "3",
                "1.27",
            ]
        )
        self.assertEqual(code, 2)
        summary = (out / "summary.md").read_text(encoding="utf-8")
        self.assertIn(f"`{run_name(target)}`", summary)
        self.assertIn("confirmed regression", summary)

    def test_apparent_improvement_that_reverses_still_fails(self):
        # Flagged as an improvement by the initial pass, then regressed in three
        # of four confirmation rounds. Regression hits are counted independently
        # of the initial direction precisely so this cannot slip through.
        target = BENCHMARKS[4]
        candidate = {name: 100.0 for name in BENCHMARKS}
        candidate[target] = 50.0
        arguments = self.three_way(100.0, candidate, 40.0)
        code, _, out = self.compare(
            arguments
            + [
                "--confirm-runs",
                "4",
                "--confirm-quorum",
                "3",
                "--confirmation",
                run_name(target),
                "0",
                "3",
                "1.20",
            ]
        )
        self.assertEqual(code, 2)
        summary = (out / "summary.md").read_text(encoding="utf-8")
        self.assertIn("confirmed regression", summary)

    def test_gate_can_be_disabled_without_changing_the_report(self):
        target = BENCHMARKS[2]
        arguments = self.three_way(100.0, self.regressed_candidate(target), 40.0)
        confirmation = [
            "--confirm-runs",
            "4",
            "--confirm-quorum",
            "3",
            "--confirmation",
            run_name(target),
            "4",
            "4",
            "1.28",
        ]
        code, _, out = self.compare(
            arguments + confirmation + ["--no-fail-on-regression"],
            output_dir=self.root / "ungated",
        )
        self.assertEqual(code, 0)
        self.assertIn("❌", (out / "summary.md").read_text(encoding="utf-8"))


class PlanTests(ComparatorTestCase):
    """The plan is what the runner re-measures, so an id missing from it is a
    benchmark that silently skips confirmation."""

    def test_plan_lists_regressions_and_caps_improvements(self):
        candidate = {name: 100.0 for name in BENCHMARKS}
        candidate[BENCHMARKS[0]] = 130.0
        for benchmark_id in BENCHMARKS[8:13]:
            candidate[benchmark_id] = 50.0
        arguments = self.three_way(100.0, candidate, 40.0)
        plan_path = self.root / "plan.txt"
        code, _, _ = self.compare(
            arguments
            + ["--plan-out", str(plan_path), "--no-summary", "--no-fail-on-regression"]
        )
        self.assertEqual(code, 0)
        entries = [
            line.split("\t")
            for line in plan_path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        kinds = [entry[0] for entry in entries]
        names = [entry[1] for entry in entries]
        self.assertEqual(kinds.count("regression"), 1)
        # Five apparent improvements, capped at the default three.
        self.assertEqual(kinds.count("improvement"), 3)
        self.assertIn(run_name(BENCHMARKS[0]), names)

    def test_ratios_output_covers_every_benchmark(self):
        arguments = self.three_way(100.0, 100.0, 40.0)
        ratios_path = self.root / "ratios.txt"
        code, _, _ = self.compare(
            arguments
            + ["--ratios-out", str(ratios_path), "--no-summary"]
        )
        self.assertEqual(code, 0)
        names = [
            line.split("\t")[0]
            for line in ratios_path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        self.assertEqual(sorted(names), sorted(run_name(x) for x in BENCHMARKS))


class MismatchTests(ComparatorTestCase):
    """A partial comparison can hide a regression, so it must be refused."""

    def test_candidate_missing_a_benchmark_is_rejected(self):
        baseline = self.write("baseline.json", make_document(BENCHMARKS, 100.0))
        candidate = self.write(
            "candidate.json", make_document(BENCHMARKS[:-1], 100.0)
        )
        code, output, _ = self.compare(
            ["--baseline", str(baseline), "--candidate", str(candidate)]
        )
        self.assertEqual(code, 1)
        self.assertIn("benchmark sets differ", output)

    def test_reference_missing_a_whole_family_is_rejected(self):
        baseline = self.write("baseline.json", make_document(BENCHMARKS, 100.0))
        candidate = self.write("candidate.json", make_document(BENCHMARKS, 100.0))
        reference = self.write(
            "reference.json",
            make_document(
                tuple(n for n in BENCHMARKS if not n.startswith("getdata/")), 40.0
            ),
        )
        code, output, _ = self.compare(
            [
                "--baseline",
                str(baseline),
                "--candidate",
                str(candidate),
                "--reference",
                str(reference),
            ]
        )
        self.assertEqual(code, 1)
        self.assertIn("benchmark sets differ", output)

    def test_confirmation_for_an_unknown_benchmark_is_rejected(self):
        arguments = self.three_way(100.0, 100.0, 40.0)
        code, output, _ = self.compare(
            arguments
            + [
                "--confirm-runs",
                "4",
                "--confirmation",
                "fetch/does_not_exist/bound_rowset_1000",
                "4",
                "4",
                "1.2",
            ]
        )
        self.assertEqual(code, 1)
        self.assertIn("unknown benchmarks", output)

    def test_confirmation_hits_above_the_round_count_are_rejected(self):
        arguments = self.three_way(100.0, 100.0, 40.0)
        code, output, _ = self.compare(
            arguments
            + [
                "--confirm-runs",
                "4",
                "--confirmation",
                run_name(BENCHMARKS[0]),
                "9",
                "9",
                "1.2",
            ]
        )
        self.assertEqual(code, 1)
        self.assertIn("exceed --confirm-runs", output)


class LegacyTwoWayTests(ComparatorTestCase):
    """The per-round confirmation comparison still runs without a reference leg,
    so the two-way contract has to keep working."""

    def test_two_way_report_is_produced(self):
        baseline = self.write("baseline.json", make_document(BENCHMARKS, 100.0))
        candidate = self.write("candidate.json", make_document(BENCHMARKS, 90.0))
        code, _, out = self.compare(
            [
                "--baseline",
                str(baseline),
                "--candidate",
                str(candidate),
                "--repetitions",
                str(REPETITIONS),
            ]
        )
        self.assertEqual(code, 0)
        summary = (out / "summary.md").read_text(encoding="utf-8")
        self.assertIn("Baseline median", summary)
        self.assertNotIn("Reference gap", summary)
        self.assertIn("10.00% lower wall time", summary)

    def test_two_way_gate_still_fires(self):
        baseline = self.write("baseline.json", make_document(BENCHMARKS, 100.0))
        candidate = self.write("candidate.json", make_document(BENCHMARKS, 130.0))
        code, _, _ = self.compare(
            ["--baseline", str(baseline), "--candidate", str(candidate)]
        )
        self.assertEqual(code, 2)


class OfficialComparatorTests(ComparatorTestCase):
    """Same policy, but with Google Benchmark's own tool doing the pairwise math.

    Skipped unless a fetched v1.9.1 checkout and its NumPy/SciPy imports are both
    available, because that is a build artifact rather than a repo file.
    """

    def setUp(self):
        super().setUp()
        candidates = list(REPO_ROOT.glob("target/**/googlebenchmark-src/tools/compare.py"))
        if not candidates:
            self.skipTest("no fetched Google Benchmark checkout in target/")
        self.gbench_compare = candidates[0]
        probe = subprocess.run(
            [sys.executable, "-c", "import numpy, scipy"],
            capture_output=True,
            check=False,
        )
        if probe.returncode != 0:
            self.skipTest("NumPy/SciPy not importable for gbench's comparator")

    def test_official_medians_cover_the_whole_set(self):
        arguments = self.three_way(100.0, 110.0, 40.0)
        code, output, out = self.compare(
            arguments
            + ["--gbench-compare", str(self.gbench_compare), "--no-fail-on-regression"]
        )
        self.assertEqual(code, 0, output)
        report = json.loads((out / "comparison.json").read_text(encoding="utf-8"))
        self.assertEqual(len(report["benchmarks"]), len(BENCHMARKS))
        for row in report["benchmarks"]:
            self.assertAlmostEqual(row["initial_ratio"], 1.1, places=3)
        self.assertTrue((out / "gbench-baseline-vs-candidate.json").is_file())
        self.assertTrue((out / "gbench-reference-vs-candidate.txt").is_file())


if __name__ == "__main__":
    unittest.main()

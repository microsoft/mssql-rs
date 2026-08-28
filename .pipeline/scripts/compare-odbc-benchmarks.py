# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Compare raw Google Benchmark samples from ODBC driver builds."""

import argparse
import json
import math
import statistics
import sys
from pathlib import Path


TIME_TO_SECONDS = {
    "ns": 1e-9,
    "us": 1e-6,
    "ms": 1e-3,
    "s": 1.0,
}

RATE_FIELDS = ("rows_per_second", "cells_per_second", "logical_bytes_per_second")


def read_samples(path):
    with path.open(encoding="utf-8-sig") as stream:
        document = json.load(stream)
    grouped = {}
    for record in document.get("benchmarks", []):
        if record.get("aggregate_name") is not None:
            continue
        if record.get("run_type", "iteration") != "iteration":
            continue

        name = record.get("run_name") or record.get("name")
        unit = record.get("time_unit")
        if not name or unit not in TIME_TO_SECONDS:
            raise ValueError(f"{path}: benchmark record has no usable name/time_unit")

        sample = {
            "real_seconds": float(record["real_time"]) * TIME_TO_SECONDS[unit],
        }
        for field in RATE_FIELDS:
            value = record.get(field)
            if isinstance(value, (int, float)) and math.isfinite(value):
                sample[field] = float(value)
        grouped.setdefault(name, []).append(sample)
    return grouped


def load_samples(paths):
    grouped = {}
    for path in paths:
        for name, samples in read_samples(path).items():
            grouped.setdefault(name, []).extend(samples)
    if not grouped:
        joined = ", ".join(str(path) for path in paths)
        raise ValueError(f"{joined}: no raw benchmark iteration samples found")
    return grouped


def describe_inputs(paths):
    return ", ".join(f"`{path.name}`" for path in paths)


def median_field(samples, field):
    values = [sample[field] for sample in samples if field in sample]
    return statistics.median(values) if values else None


def format_rate(value):
    if value is None:
        return "-"
    for scale, suffix in ((1e9, "G"), (1e6, "M"), (1e3, "K")):
        if abs(value) >= scale:
            return f"{value / scale:.2f}{suffix}"
    return f"{value:.2f}"


def require_matching_benchmarks(baseline, other, other_name):
    baseline_names = set(baseline)
    other_names = set(other)
    if baseline_names != other_names:
        missing_other = sorted(baseline_names - other_names)
        missing_baseline = sorted(other_names - baseline_names)
        raise ValueError(
            "benchmark sets differ: "
            f"missing from {other_name}={missing_other}, "
            f"missing from baseline={missing_baseline}"
        )


def compare(baseline, candidate, threshold, reference=None):
    require_matching_benchmarks(baseline, candidate, "candidate")
    if reference is not None:
        require_matching_benchmarks(baseline, reference, "reference")

    results = []
    for name in sorted(baseline):
        base_samples = baseline[name]
        candidate_samples = candidate[name]
        base_seconds = median_field(base_samples, "real_seconds")
        candidate_seconds = median_field(candidate_samples, "real_seconds")
        if base_seconds <= 0 or candidate_seconds <= 0:
            raise ValueError(f"{name}: benchmark duration must be greater than zero")
        ratio = candidate_seconds / base_seconds
        rates = {
            field: median_field(candidate_samples, field) for field in RATE_FIELDS
        }
        result = {
            "name": name,
            "baseline_samples": len(base_samples),
            "candidate_samples": len(candidate_samples),
            "baseline_median_seconds": base_seconds,
            "candidate_median_seconds": candidate_seconds,
            "candidate_ratio": ratio,
            "change_percent": (ratio - 1.0) * 100.0,
            "regression": ratio >= threshold,
            **rates,
        }
        if reference is not None:
            reference_samples = reference[name]
            reference_seconds = median_field(reference_samples, "real_seconds")
            if reference_seconds <= 0:
                raise ValueError(f"{name}: benchmark duration must be greater than zero")
            reference_ratio = candidate_seconds / reference_seconds
            result.update(
                {
                    "reference_samples": len(reference_samples),
                    "reference_median_seconds": reference_seconds,
                    "candidate_vs_reference_ratio": reference_ratio,
                    "candidate_vs_reference_change_percent": (
                        reference_ratio - 1.0
                    )
                    * 100.0,
                    **{
                        f"reference_{field}": median_field(reference_samples, field)
                        for field in RATE_FIELDS
                    },
                }
            )
        results.append(result)
    return results


def comparison_text(results, reference_label=None):
    if reference_label is not None:
        lines = [
            f"Benchmark comparison (baseline / {reference_label} / candidate)",
            "",
            (
                f"{'benchmark':56} {'base ms':>12} {'reference ms':>14} "
                f"{'candidate ms':>14} {'vs base':>10} {'vs reference':>13}"
            ),
        ]
        for result in results:
            lines.append(
                f"{result['name'][:56]:56} "
                f"{result['baseline_median_seconds'] * 1000:12.2f} "
                f"{result['reference_median_seconds'] * 1000:14.2f} "
                f"{result['candidate_median_seconds'] * 1000:14.2f} "
                f"{result['change_percent']:+9.2f}% "
                f"{result['candidate_vs_reference_change_percent']:+12.2f}%"
            )
        return "\n".join(lines) + "\n"

    lines = [
        "Benchmark comparison (baseline -> candidate)",
        "",
        f"{'benchmark':68} {'base ms':>12} {'candidate ms':>14} {'change':>10}",
    ]
    for result in results:
        lines.append(
            f"{result['name'][:68]:68} "
            f"{result['baseline_median_seconds'] * 1000:12.2f} "
            f"{result['candidate_median_seconds'] * 1000:14.2f} "
            f"{result['change_percent']:+9.2f}%"
        )
    return "\n".join(lines) + "\n"


def summary_markdown(
    results,
    threshold,
    baseline_paths,
    candidate_paths,
    reference_paths=None,
    reference_label=None,
):
    regressions = [result for result in results if result["regression"]]
    if regressions:
        verdict = (
            f"❌ {len(regressions)} benchmark(s) were at least "
            f"{(threshold - 1.0) * 100:.0f}% slower than baseline."
        )
    else:
        verdict = (
            f"✅ No benchmark was at least "
            f"{(threshold - 1.0) * 100:.0f}% slower than baseline."
        )

    lines = ["# ODBC performance comparison", "", verdict, ""]
    if reference_paths:
        lines.extend(
            [
                (
                    f"| Benchmark | Baseline median | {reference_label} median | "
                    "Candidate median | Candidate vs baseline | "
                    f"Candidate vs {reference_label} | Candidate rows/s | "
                    "Candidate cells/s |"
                ),
                "|---|---:|---:|---:|---:|---:|---:|---:|",
            ]
        )
        for result in results:
            lines.append(
                "| `{name}` | {base:.2f} ms | {reference:.2f} ms | "
                "{candidate:.2f} ms | {base_change:+.2f}% | "
                "{reference_change:+.2f}% | {rows} | {cells} |".format(
                    name=result["name"],
                    base=result["baseline_median_seconds"] * 1000,
                    reference=result["reference_median_seconds"] * 1000,
                    candidate=result["candidate_median_seconds"] * 1000,
                    base_change=result["change_percent"],
                    reference_change=result[
                        "candidate_vs_reference_change_percent"
                    ],
                    rows=format_rate(result["rows_per_second"]),
                    cells=format_rate(result["cells_per_second"]),
                )
            )
    else:
        lines.extend(
            [
                "| Benchmark | Baseline median | Candidate median | Change | Candidate rows/s | Candidate cells/s |",
                "|---|---:|---:|---:|---:|---:|",
            ]
        )
        for result in results:
            lines.append(
                "| `{name}` | {base:.2f} ms | {candidate:.2f} ms | "
                "{change:+.2f}% | {rows} | {cells} |".format(
                    name=result["name"],
                    base=result["baseline_median_seconds"] * 1000,
                    candidate=result["candidate_median_seconds"] * 1000,
                    change=result["change_percent"],
                    rows=format_rate(result["rows_per_second"]),
                    cells=format_rate(result["cells_per_second"]),
                )
            )
    lines.extend(
        [
            "",
            f"Baseline samples: {describe_inputs(baseline_paths)}",
            f"Candidate samples: {describe_inputs(candidate_paths)}",
        ]
    )
    if reference_paths:
        lines.append(
            f"{reference_label} samples: {describe_inputs(reference_paths)}"
        )
    lines.append("")
    if reference_paths:
        lines.append(
            "The gate is based on median complete-result wall time from the "
            "candidate and baseline samples. Reference results are informational."
        )
    else:
        lines.append(
            "The gate is based on median complete-result wall time. Throughput "
            "counters come from the candidate samples."
        )
    return "\n".join(lines) + "\n"


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", required=True, type=Path, action="append")
    parser.add_argument("--candidate", required=True, type=Path, action="append")
    parser.add_argument("--reference", type=Path, action="append")
    parser.add_argument("--reference-label", default="Reference")
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--regression-ratio", type=float, default=1.10)
    parser.add_argument("--fail-on-regression", action="store_true")
    return parser.parse_args()


def main():
    args = parse_args()
    if not math.isfinite(args.regression_ratio) or args.regression_ratio <= 1.0:
        raise ValueError("--regression-ratio must be a finite value greater than 1")
    if args.reference and not args.reference_label.strip():
        raise ValueError("--reference-label must not be empty")

    baseline = load_samples(args.baseline)
    candidate = load_samples(args.candidate)
    reference = load_samples(args.reference) if args.reference else None
    results = compare(baseline, candidate, args.regression_ratio, reference)

    args.output_dir.mkdir(parents=True, exist_ok=True)
    reference_label = args.reference_label.strip() if reference else None
    text = comparison_text(results, reference_label)
    (args.output_dir / "comparison.txt").write_text(text, encoding="utf-8")
    (args.output_dir / "summary.md").write_text(
        summary_markdown(
            results,
            args.regression_ratio,
            args.baseline,
            args.candidate,
            args.reference,
            reference_label,
        ),
        encoding="utf-8",
    )
    output = {
        "regression_ratio": args.regression_ratio,
        "benchmarks": results,
    }
    if reference_label is not None:
        output["reference_label"] = reference_label
    (args.output_dir / "comparison.json").write_text(
        json.dumps(output, indent=2) + "\n",
        encoding="utf-8",
    )
    sys.stdout.write(text)

    if args.fail_on_regression and any(result["regression"] for result in results):
        return 2
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)

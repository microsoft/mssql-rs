# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Repo policy and reporting wrapper around Google Benchmark's own comparator.

Google Benchmark v1.9.1 ships ``tools/compare.py`` and ``gbench.report``; the
fetched copy in the CMake build tree owns the pairwise math. Pass
``--gbench-compare`` and this script only supplies what the official tool does
not: exact benchmark-set validation, the combined three-way report, custom
throughput counters and pins, the regression gate, targeted confirmation
overrides, and Azure DevOps Markdown.

The Mann-Whitney U test is disabled explicitly (``--no-utest``). The lab runs
five repetitions, which is below the nine that Google Benchmark documents as the
minimum for a meaningful U test, so its p-values would be reported without being
trustworthy. Reproduction is established by the targeted confirmation rounds
instead.
"""

import argparse
import json
import math
import statistics
import subprocess
import sys
from pathlib import Path


TIME_TO_SECONDS = {
    "ns": 1e-9,
    "us": 1e-6,
    "ms": 1e-3,
    "s": 1.0,
}

RATE_FIELDS = ("rows_per_second", "cells_per_second", "logical_bytes_per_second")
BAR_PERCENT_PER_SQUARE = 5.0
BAR_MAX_SQUARES = 20
DEFAULT_REGRESSION_RATIO = 1.05
DEFAULT_IMPROVEMENT_MAX = 3
GEOMEAN_ENTRY = "OVERALL_GEOMEAN"
PAIRED_FORMAT = "mssql-odbc-bench-paired/1"
PAIRED_MIN_ROUNDS = 10
PAIRED_ROLES = ("candidate", "baseline", "reference")
# One-sided sign-test level a paired direction must reach before it counts.
PAIRED_SIGN_ALPHA = 0.01
PAIRED_CI_LEVEL = 0.95


def read_samples(path):
    """Read only raw iteration records so aggregate rows cannot skew the median."""
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
    """Merge scenario files into one benchmark-keyed sample set."""
    grouped = {}
    for path in paths:
        for name, samples in read_samples(path).items():
            grouped.setdefault(name, []).extend(samples)
    if not grouped:
        joined = ", ".join(str(path) for path in paths)
        raise ValueError(f"{joined}: no raw benchmark iteration samples found")
    return grouped


def merge_scenario_documents(paths, destination):
    """Join per-scenario JSON into the single document the official tool expects.

    The harness runs one scenario per process, so every file restarts
    ``family_index`` at zero. Renumbering by first appearance keeps the merged
    document shaped like a single multi-family run and keeps the merge
    reproducible for a given input order.
    """
    context = None
    records = []
    families = {}
    for path in paths:
        with path.open(encoding="utf-8-sig") as stream:
            document = json.load(stream)
        if context is None:
            context = document.get("context", {})
        for record in document.get("benchmarks", []):
            name = record.get("run_name") or record.get("name")
            if not name:
                raise ValueError(f"{path}: benchmark record has no usable name")
            merged = dict(record)
            merged["family_index"] = families.setdefault(name, len(families))
            records.append(merged)
    if not records:
        joined = ", ".join(str(path) for path in paths)
        raise ValueError(f"{joined}: no benchmark records found")
    destination.write_text(
        json.dumps({"context": context or {}, "benchmarks": records}, indent=1) + "\n",
        encoding="utf-8",
    )
    return destination


def run_official_compare(compare_script, baseline_path, candidate_path, output_dir, slug):
    """Run Google Benchmark's comparator and keep its text and JSON output."""
    dump_path = output_dir / f"gbench-{slug}.json"
    text_path = output_dir / f"gbench-{slug}.txt"
    command = [
        sys.executable,
        str(compare_script),
        "--no-color",
        "--no-utest",
        "--dump_to_json",
        str(dump_path),
        "benchmarks",
        str(baseline_path),
        str(candidate_path),
    ]
    completed = subprocess.run(
        command,
        capture_output=True,
        text=True,
        check=False,
    )
    text_path.write_text(completed.stdout + completed.stderr, encoding="utf-8")
    if completed.returncode != 0:
        raise ValueError(
            f"{compare_script}: official comparison failed "
            f"(exit {completed.returncode}); see {text_path.name}"
        )
    with dump_path.open(encoding="utf-8") as stream:
        return json.load(stream)


def official_medians(diff_report, source):
    """Take medians and ratios from the official report instead of recomputing.

    Google Benchmark emits a ``_median`` aggregate whenever a run has more than
    one repetition, and the official report carries its ratio as ``time``
    (candidate/baseline - 1). A single-repetition run has no aggregate, so fall
    back to the per-repetition entries the same report already paired up.
    """
    medians = {}
    fallback = {}
    for entry in diff_report:
        name = entry.get("name", "")
        if name == GEOMEAN_ENTRY:
            continue
        measurements = entry.get("measurements") or []
        if not measurements:
            continue
        unit = TIME_TO_SECONDS.get(entry.get("time_unit"))
        if unit is None:
            raise ValueError(f"{source}: '{name}' has an unusable time_unit")
        if entry.get("run_type") == "aggregate":
            aggregate = entry.get("aggregate_name") or ""
            if aggregate != "median":
                continue
            suffix = f"_{aggregate}"
            if not name.endswith(suffix):
                raise ValueError(f"{source}: aggregate '{name}' has an unexpected name")
            medians[name[: -len(suffix)]] = {
                "baseline_seconds": float(measurements[0]["real_time"]) * unit,
                "candidate_seconds": float(measurements[0]["real_time_other"]) * unit,
                "ratio": 1.0 + float(measurements[0]["time"]),
            }
        else:
            fallback[name] = {
                "baseline_seconds": statistics.median(
                    float(item["real_time"]) for item in measurements
                )
                * unit,
                "candidate_seconds": statistics.median(
                    float(item["real_time_other"]) for item in measurements
                )
                * unit,
                "ratio": statistics.median(
                    1.0 + float(item["time"]) for item in measurements
                ),
            }
    for name, value in fallback.items():
        medians.setdefault(name, value)
    if not medians:
        raise ValueError(f"{source}: official report contained no comparable benchmarks")
    return medians


def describe_inputs(paths):
    """Render artifact basenames without leaking agent-specific directories."""
    return ", ".join(f"`{path.name}`" for path in paths)


def median_field(samples, field):
    """Return a median only when the benchmark emitted the requested counter."""
    values = [sample[field] for sample in samples if field in sample]
    return statistics.median(values) if values else None


def format_rate(value):
    """Keep high-volume throughput columns compact enough for the Summary tab."""
    if value is None:
        return "-"
    for scale, suffix in ((1e9, "G"), (1e6, "M"), (1e3, "K")):
        if abs(value) >= scale:
            return f"{value / scale:.2f}{suffix}"
    return f"{value:.2f}"


def format_speedup(ratio):
    """Express comparator/candidate wall time as a direction-neutral speedup factor."""
    return f"{1.0 / ratio:.2f}x"


def format_time_multiple(ratio):
    """State how many times as long the candidate took when it is slower."""
    if ratio >= 1.0:
        return f"{ratio:.2f}x as long"
    return f"{1.0 / ratio:.2f}x less time"


def format_wall_time_change(change_percent):
    """Name the wall-time direction explicitly; 'percent faster' is ambiguous."""
    if change_percent < -0.005:
        return f"{abs(change_percent):.2f}% lower wall time"
    if change_percent > 0.005:
        return f"{change_percent:.2f}% higher wall time"
    return "0.00% wall-time change"


def bar_squares(change_percent):
    """Scale large ODBC deltas to a readable chart instead of flooding the report."""
    if abs(change_percent) < 0.05:
        return 0
    rounded = math.floor(abs(change_percent) / BAR_PERCENT_PER_SQUARE + 0.5)
    return min(BAR_MAX_SQUARES, rounded)


def require_matching_benchmarks(baseline, other, other_name):
    """Reject partial comparisons because missing workloads can hide regressions."""
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


def medians_from_samples(baseline, candidate):
    """Compute the pairwise medians used when the official comparator is absent."""
    medians = {}
    for name, base_samples in baseline.items():
        base_seconds = median_field(base_samples, "real_seconds")
        candidate_seconds = median_field(candidate[name], "real_seconds")
        if base_seconds <= 0 or candidate_seconds <= 0:
            raise ValueError(f"{name}: benchmark duration must be greater than zero")
        medians[name] = {
            "baseline_seconds": base_seconds,
            "candidate_seconds": candidate_seconds,
            "ratio": candidate_seconds / base_seconds,
        }
    return medians


def require_official_coverage(medians, expected, source):
    """The official report must cover the exact validated set, or the gate is blind."""
    missing = sorted(set(expected) - set(medians))
    if missing:
        raise ValueError(f"{source}: official report is missing benchmarks {missing}")


def compare(
    baseline,
    candidate,
    threshold,
    reference=None,
    baseline_medians=None,
    reference_medians=None,
    improvement_ratio=None,
    confirmations=None,
    confirm_runs=None,
    confirm_quorum=None,
):
    """Compare median wall time, then let confirmation rounds own the verdict."""
    require_matching_benchmarks(baseline, candidate, "candidate")
    if reference is not None:
        require_matching_benchmarks(baseline, reference, "reference")

    if baseline_medians is None:
        baseline_medians = medians_from_samples(baseline, candidate)
    require_official_coverage(baseline_medians, baseline, "baseline vs candidate")
    if reference is not None and reference_medians is None:
        reference_medians = medians_from_samples(reference, candidate)
    if reference_medians is not None:
        require_official_coverage(reference_medians, baseline, "reference vs candidate")

    improvement_ratio = improvement_ratio or threshold
    confirmations = confirmations or {}
    unknown = sorted(set(confirmations) - set(baseline))
    if unknown:
        raise ValueError(f"confirmation results name unknown benchmarks: {unknown}")

    results = []
    for name in sorted(baseline):
        base_samples = baseline[name]
        candidate_samples = candidate[name]
        pair = baseline_medians[name]
        initial_ratio = pair["ratio"]
        if not math.isfinite(initial_ratio) or initial_ratio <= 0:
            raise ValueError(f"{name}: benchmark duration must be greater than zero")
        initial_regression = initial_ratio >= threshold
        initial_improvement = initial_ratio <= 1.0 / improvement_ratio
        confirmation = confirmations.get(name)
        headline_ratio = (
            confirmation["ratio"] if confirmation is not None else initial_ratio
        )
        if confirm_runs is not None:
            regression = (
                confirmation is not None
                and confirmation["regression_hits"] >= confirm_quorum
            )
        else:
            regression = initial_regression
        result = {
            "name": name,
            "baseline_samples": len(base_samples),
            "candidate_samples": len(candidate_samples),
            "baseline_median_seconds": pair["baseline_seconds"],
            "candidate_median_seconds": pair["candidate_seconds"],
            "candidate_ratio": headline_ratio,
            "change_percent": (headline_ratio - 1.0) * 100.0,
            "initial_ratio": initial_ratio,
            "initial_change_percent": (initial_ratio - 1.0) * 100.0,
            "initial_regression": initial_regression,
            "initial_improvement": initial_improvement,
            "regression": regression,
            **{field: median_field(candidate_samples, field) for field in RATE_FIELDS},
        }
        if confirmation is not None:
            result.update(
                {
                    "confirmation_hits": confirmation["hits"],
                    "confirmation_regression_hits": confirmation["regression_hits"],
                    "confirmation_runs": confirm_runs,
                    "confirmation_ratio": confirmation["ratio"],
                    "confirmed": confirmation["hits"] >= confirm_quorum,
                    "confirmed_regression": regression,
                }
            )
        if reference is not None:
            reference_samples = reference[name]
            reference_pair = reference_medians[name]
            reference_ratio = reference_pair["ratio"]
            if not math.isfinite(reference_ratio) or reference_ratio <= 0:
                raise ValueError(f"{name}: benchmark duration must be greater than zero")
            result.update(
                {
                    "reference_samples": len(reference_samples),
                    "reference_median_seconds": reference_pair["baseline_seconds"],
                    "candidate_vs_reference_ratio": reference_ratio,
                    "candidate_vs_reference_change_percent": (reference_ratio - 1.0)
                    * 100.0,
                    **{
                        f"reference_{field}": median_field(reference_samples, field)
                        for field in RATE_FIELDS
                    },
                }
            )
        results.append(result)
    if confirm_runs is not None:
        # An initial regression with no confirmation data would otherwise clear the
        # gate silently, which is the one failure mode this flow must not have.
        missing = sorted(
            result["name"]
            for result in results
            if result["initial_regression"] and "confirmation_hits" not in result
        )
        if missing:
            raise ValueError(f"confirmation results are missing for {missing}")
    return results


def confirmation_plan(results, improvement_max):
    """List what the runner must re-measure: every regression, capped improvements.

    Regressions are self-limiting because they fail the run, but a single hot-path
    change can turn every benchmark green at once, and each verified entry costs a
    full set of paired re-runs. Keep the largest apparent wins and report the rest
    unverified.
    """
    plan = [
        ("regression", result["name"], result["initial_ratio"])
        for result in results
        if result["initial_regression"]
    ]
    improvements = sorted(
        (result for result in results if result["initial_improvement"]),
        key=lambda result: result["initial_ratio"],
    )
    kept = improvements[:improvement_max]
    plan.extend(
        ("improvement", result["name"], result["initial_ratio"]) for result in kept
    )
    return plan, len(improvements) - len(kept)


def binomial_upper_tail(count, successes):
    """P(X >= successes) for X ~ Binomial(count, 1/2)."""
    if successes <= 0:
        return 1.0
    total = sum(math.comb(count, value) for value in range(successes, count + 1))
    return total / 2**count


def sign_rounds_required(count, alpha=PAIRED_SIGN_ALPHA):
    """Fewest same-direction rounds that a one-sided sign test accepts at alpha."""
    for successes in range(count // 2 + 1, count + 1):
        if binomial_upper_tail(count, successes) <= alpha:
            return successes
    return None


def median_confidence_interval(sorted_values, level=PAIRED_CI_LEVEL):
    """Distribution-free order-statistic interval for a median.

    Uses the largest k with P(X <= k - 1) <= (1 - level) / 2, X ~ Bin(n, 1/2),
    and returns the k-th smallest and k-th largest values.
    """
    count = len(sorted_values)
    tail = (1.0 - level) / 2.0
    rank = 0
    for candidate in range(1, count // 2 + 1):
        if 1.0 - binomial_upper_tail(count, candidate) <= tail:
            rank = candidate
        else:
            break
    if rank == 0:
        return sorted_values[0], sorted_values[-1]
    return sorted_values[rank - 1], sorted_values[count - rank]


def load_paired(path, unpaired_names):
    """Read and validate the paired harness document; any doubt is an error."""
    with path.open(encoding="utf-8-sig") as stream:
        document = json.load(stream)
    if document.get("format") != PAIRED_FORMAT:
        raise ValueError(f"{path}: expected format '{PAIRED_FORMAT}'")
    rounds = document.get("rounds")
    if not isinstance(rounds, int) or rounds < PAIRED_MIN_ROUNDS:
        raise ValueError(f"{path}: paired runs need at least {PAIRED_MIN_ROUNDS} rounds")
    delay = document.get("candidate_delay_ns")
    if not isinstance(delay, int) or delay < 0:
        raise ValueError(f"{path}: candidate_delay_ns must be a non-negative integer")
    roles = [leg.get("role") for leg in document.get("legs") or []]
    if len(set(roles)) != len(roles) or not set(roles) <= set(PAIRED_ROLES):
        raise ValueError(f"{path}: leg roles must be distinct values of {PAIRED_ROLES}")
    if "candidate" not in roles or "baseline" not in roles:
        raise ValueError(f"{path}: paired runs need candidate and baseline legs")
    benchmarks = document.get("benchmarks") or []
    if not benchmarks:
        raise ValueError(f"{path}: no paired benchmarks found")
    names = set()
    for entry in benchmarks:
        name = entry.get("name")
        if not isinstance(name, str) or not name or name in names:
            raise ValueError(f"{path}: paired benchmark names must be unique strings")
        if name in unpaired_names:
            raise ValueError(f"{path}: paired benchmark '{name}' collides with a sample id")
        names.add(name)
        calls = entry.get("calls_per_batch")
        if not isinstance(calls, int) or calls < 1:
            raise ValueError(f"{path}: '{name}' needs a positive calls_per_batch")
        seconds = entry.get("seconds") or {}
        if set(seconds) != set(roles):
            raise ValueError(f"{path}: '{name}' must carry times for exactly {roles}")
        for role, values in seconds.items():
            if (
                not isinstance(values, list)
                or len(values) != rounds
                or not all(
                    isinstance(value, (int, float))
                    and math.isfinite(value)
                    and value > 0
                    for value in values
                )
            ):
                raise ValueError(
                    f"{path}: '{name}' {role} needs {rounds} positive batch times"
                )
    return document


def paired_results(document, threshold, improvement_ratio):
    """Turn per-round batch times into the paired verdicts.

    Each round's candidate/baseline ratio is one paired observation. The gate
    needs both a material effect (median ratio at or above the regression ratio)
    and a consistent one (the candidate slower in enough rounds for a one-sided
    sign test at PAIRED_SIGN_ALPHA), so neither one bad round nor a tiny but
    consistent shift can fail the run.
    """
    rounds = document["rounds"]
    results = []
    for entry in document["benchmarks"]:
        seconds = entry["seconds"]
        calls = entry["calls_per_batch"]
        ratios = [
            candidate / baseline
            for candidate, baseline in zip(seconds["candidate"], seconds["baseline"])
        ]
        ordered = sorted(ratios)
        ratio = statistics.median(ordered)
        quartiles = statistics.quantiles(ordered, n=4, method="inclusive")
        ci_low, ci_high = median_confidence_interval(ordered)
        faster = sum(1 for value in ratios if value < 1.0)
        slower = sum(1 for value in ratios if value > 1.0)
        decided = faster + slower
        required = sign_rounds_required(decided)
        significant_faster = required is not None and faster >= required
        significant_slower = required is not None and slower >= required
        if significant_faster:
            direction = "faster"
        elif significant_slower:
            direction = "slower"
        else:
            direction = "no significant difference"
        baseline_seconds = statistics.median(seconds["baseline"])
        candidate_seconds = statistics.median(seconds["candidate"])
        result = {
            "name": entry["name"],
            "mode": "paired",
            "rounds": rounds,
            "calls_per_batch": calls,
            "baseline_median_seconds": baseline_seconds,
            "candidate_median_seconds": candidate_seconds,
            "baseline_us_per_call": baseline_seconds * 1e6 / calls,
            "candidate_us_per_call": candidate_seconds * 1e6 / calls,
            "candidate_ratio": ratio,
            "change_percent": (ratio - 1.0) * 100.0,
            "ratio_q1": quartiles[0],
            "ratio_q3": quartiles[2],
            "ratio_iqr": quartiles[2] - quartiles[0],
            "ratio_ci95_low": ci_low,
            "ratio_ci95_high": ci_high,
            "candidate_faster_rounds": faster,
            "candidate_slower_rounds": slower,
            "sign_rounds_required": required,
            "sign_test_p_two_sided": min(
                1.0, 2.0 * binomial_upper_tail(decided, max(faster, slower))
            ),
            "direction": direction,
            "regression": ratio >= threshold and significant_slower,
            "verified_improvement": ratio <= 1.0 / improvement_ratio
            and significant_faster,
        }
        if "reference" in seconds:
            reference_seconds = statistics.median(seconds["reference"])
            reference_ratio = statistics.median(
                candidate / reference
                for candidate, reference in zip(
                    seconds["candidate"], seconds["reference"]
                )
            )
            result.update(
                {
                    "reference_median_seconds": reference_seconds,
                    "reference_us_per_call": reference_seconds * 1e6 / calls,
                    "candidate_vs_reference_ratio": reference_ratio,
                    "candidate_vs_reference_change_percent": (reference_ratio - 1.0)
                    * 100.0,
                }
            )
        results.append(result)
    return results


def paired_text(paired):
    """Plain-text lines for the paired entries in comparison.txt."""
    lines = [
        "",
        "Paired interleaved latency (median per-round candidate/baseline ratio)",
        "",
        f"{'benchmark':48} {'base us':>9} {'cand us':>9} {'ratio':>8} "
        f"{'95% CI':>15} {'faster/slower':>14}",
    ]
    for result in paired:
        lines.append(
            f"{result['name'][:48]:48} "
            f"{result['baseline_us_per_call']:9.2f} "
            f"{result['candidate_us_per_call']:9.2f} "
            f"{result['candidate_ratio']:8.4f} "
            f"{result['ratio_ci95_low']:7.4f}-{result['ratio_ci95_high']:<7.4f} "
            f"{result['candidate_faster_rounds']:>6}/{result['candidate_slower_rounds']:<7}"
        )
    return "\n".join(lines) + "\n"


def paired_section(paired, threshold, reference_label, candidate_delay_ns):
    """Markdown for the paired entries, including the rule they gate on."""
    rounds = paired[0]["rounds"]
    required = paired[0]["sign_rounds_required"]
    lines = ["", "### Paired interleaved latency (candidate vs pinned baseline)", ""]
    if candidate_delay_ns:
        lines.extend(
            [
                f"> ⚠️ **Harness self-test run:** the candidate leg was slowed by "
                f"{candidate_delay_ns} ns per call. These numbers do not describe "
                "a driver.",
                "",
            ]
        )
    lines.append(
        "One process holds a connection through every driver and alternates "
        "short batches of calls between them, swapping the candidate and "
        "baseline order every round, so the drivers share server state, machine "
        "load, and clock regime within a fraction of a second. Each round gives "
        "one candidate/baseline ratio; the "
        f"headline is the median of **{rounds} rounds**, with a distribution-free "
        "95% interval for that median."
    )
    lines.append("")
    lines.append(
        f"These entries gate on their own rule and are not re-measured: a "
        f"regression needs a median ratio of at least **{threshold:.2f}** *and* "
        f"the candidate slower in at least **{required} of {rounds}** rounds "
        f"(one-sided sign test, p ≤ {PAIRED_SIGN_ALPHA}). A direction is reported "
        "as faster or slower whenever the sign test alone passes, whatever its "
        "size."
    )
    has_reference = all("candidate_vs_reference_ratio" in result for result in paired)
    header = (
        "| Benchmark | Rounds × calls | Baseline µs/call | Candidate µs/call | "
        "Median ratio (95% CI) | IQR | Faster / slower rounds | Result |"
    )
    divider = "|---|--:|--:|--:|--:|--:|:--:|---|"
    if has_reference:
        header += f" Candidate / {reference_label} |"
        divider += "--:|"
    lines.extend(["", header, divider])
    for result in paired:
        if result["regression"]:
            verdict = "❌ regression"
        elif result["verified_improvement"]:
            verdict = "✅ verified improvement"
        else:
            verdict = result["direction"]
        verdict += f" (p={result['sign_test_p_two_sided']:.2g})"
        row = (
            f"| `{result['name']}` | {result['rounds']} × {result['calls_per_batch']} | "
            f"{result['baseline_us_per_call']:.2f} | "
            f"{result['candidate_us_per_call']:.2f} | "
            f"{result['candidate_ratio']:.4f} "
            f"({result['ratio_ci95_low']:.4f}–{result['ratio_ci95_high']:.4f}) | "
            f"{result['ratio_iqr']:.4f} | "
            f"{result['candidate_faster_rounds']} / {result['candidate_slower_rounds']} | "
            f"{verdict} |"
        )
        if has_reference:
            row += f" {result['candidate_vs_reference_ratio']:.4f} |"
        lines.append(row)
    return lines


def comparison_text(results, reference_label=None):
    """Produce a compact plain-text artifact suited to step logs and downloads."""
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
                f"{result['initial_change_percent']:+9.2f}% "
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
            f"{result['initial_change_percent']:+9.2f}%"
        )
    return "\n".join(lines) + "\n"


def diverging_bar_table(results, change_key, ratio_key, mark_confirmed=False):
    """Render lower wall time left/green and higher wall time right/red."""
    lines = [
        "| Benchmark | lower wall time ◄ | Wall-time change | ► higher wall time | Speedup factor |",
        "|---|--:|:--:|:--|--:|",
    ]
    for result in sorted(results, key=lambda item: item[change_key]):
        change = result[change_key]
        squares = bar_squares(change)
        lower = "🟩" * squares if change < -0.05 else ""
        higher = "🟥" * squares if change > 0.05 else ""
        mark = ""
        if mark_confirmed and "confirmation_ratio" in result:
            if result.get("confirmed_regression"):
                status = "confirmed regression"
                hits = result["confirmation_regression_hits"]
            elif result.get("confirmed"):
                status = "verified improvement"
                hits = result["confirmation_hits"]
            else:
                status = "not confirmed"
                hits = result["confirmation_hits"]
            mark = (
                f" ⟳ ({hits}/"
                f"{result['confirmation_runs']}; {status})"
            )
        lines.append(
            f"| `{result['name']}`{mark} | {lower} | "
            f"{format_wall_time_change(change)} | {higher} | "
            f"{format_speedup(result[ratio_key])} |"
        )
    return lines


def add_section_spacing(lines):
    """Keep Azure DevOps from visually collapsing adjacent report sections."""
    spaced = []
    for line in lines:
        if line.startswith("### "):
            while spaced and spaced[-1] == "":
                spaced.pop()
            spaced.extend(["", "<br>", "", line])
        else:
            spaced.append(line)
    return spaced


def confirmation_section(
    results,
    threshold,
    improvement_ratio,
    repetitions,
    confirm_runs,
    confirm_quorum,
    skipped,
):
    """Explain what the initial pass flagged and what the re-runs actually held."""
    lines = []
    initial_regressions = [r for r in results if r["initial_regression"]]
    initial_improvements = [r for r in results if r["initial_improvement"]]
    confirmed = [r for r in results if r.get("confirmed_regression")]
    verified = [
        r
        for r in results
        if r.get("confirmed") and r["initial_improvement"] and not r["regression"]
    ]
    cleared_regressions = [
        r
        for r in initial_regressions
        if "confirmation_hits" in r and not r["regression"]
    ]
    cleared_improvements = [
        r
        for r in initial_improvements
        if "confirmation_hits" in r and not r["confirmed"] and not r["regression"]
    ]
    improvement_percent = (1.0 - 1.0 / improvement_ratio) * 100
    sample_description = (
        f"{repetitions}-sample" if repetitions is not None else "raw-sample"
    )

    lines.extend(["", "### Initial pass vs confirmation", ""])
    lines.append(
        f"The initial {sample_description} median flagged **{len(initial_regressions)} "
        f"regression(s)** and **{len(initial_improvements)} large apparent "
        "improvement(s)**."
    )
    if confirm_runs is None:
        lines.extend(["", "_No confirmation rounds were run for this comparison._"])
        return lines
    lines.append("")
    lines.append(
        f"Each flagged benchmark was then re-measured with the candidate and the "
        f"pinned mssql-odbc baseline back-to-back for **{confirm_runs} confirmation "
        f"rounds**. A result counts only when it reproduces in at least "
        f"**{confirm_quorum} of {confirm_runs}** rounds. The headline wall-time "
        f"change for a re-measured benchmark (⟳) is the median of those "
        f"{confirm_runs} confirmation ratios; the initial pass that triggered the "
        "re-measurement is deliberately excluded so the outlier under test does "
        "not vote on its own verdict."
    )
    lines.append("")
    lines.append(
        f"**{len(confirmed)} confirmed regression(s)** and "
        f"**{len(verified)} verified improvement(s)**; "
        f"{len(cleared_regressions)} initial regression(s) and "
        f"{len(cleared_improvements)} apparent improvement(s) did "
        "not reproduce and are treated as transient noise."
    )
    if skipped:
        lines.append("")
        lines.append(
            f"_{skipped} further benchmark(s) had at least "
            f"{improvement_percent:.2f}% lower wall time but were outside the "
            "re-measurement cap; they keep their initial numbers, unverified._"
        )
    if not (initial_regressions or initial_improvements):
        return lines

    lines.extend(
        [
            "",
            "| Benchmark | Flagged as | Initial change | Initial direction reproduced | Regression rounds | Confirmed change |",
            "|---|:--:|--:|:--:|:--:|--:|",
        ]
    )
    for result in sorted(
        initial_regressions + initial_improvements, key=lambda item: item["name"]
    ):
        kind = "regression" if result["initial_regression"] else "improvement"
        hits = result.get("confirmation_hits")
        rounds = "—" if hits is None else f"{hits}/{confirm_runs}"
        regression_hits = result.get("confirmation_regression_hits")
        regression_rounds = (
            "—" if regression_hits is None else f"{regression_hits}/{confirm_runs}"
        )
        confirmed_change = (
            "—"
            if "confirmation_ratio" not in result
            else format_wall_time_change((result["confirmation_ratio"] - 1.0) * 100.0)
        )
        lines.append(
            f"| `{result['name']}` | {kind} | "
            f"{format_wall_time_change(result['initial_change_percent'])} | "
            f"{rounds} | {regression_rounds} | {confirmed_change} |"
        )
    return lines


def summary_markdown(
    results,
    threshold,
    improvement_ratio,
    baseline_paths,
    candidate_paths,
    reference_paths=None,
    reference_label=None,
    baseline_commit=None,
    reference_version=None,
    repetitions=None,
    confirm_runs=None,
    confirm_quorum=None,
    improvements_skipped=0,
    official_compare=False,
    paired=None,
    paired_path=None,
    paired_delay_ns=0,
):
    """Build the single rich report uploaded by the shared PerfTest template."""
    paired = paired or []
    regressions = [result for result in results + paired if result["regression"]]
    gated = "confirmed " if confirm_runs is not None else ""
    if regressions:
        verdict = (
            f"❌ {len(regressions)} {gated}benchmark(s) had at least "
            f"{(threshold - 1.0) * 100:.0f}% higher wall time than the pinned "
            "mssql-odbc baseline."
        )
    else:
        verdict = (
            f"✅ No {gated}benchmark had at least "
            f"{(threshold - 1.0) * 100:.0f}% higher wall time than the pinned "
            "mssql-odbc baseline."
        )

    lines = [
        "# ODBC performance comparison",
        "",
        "This report contains two separate comparisons:",
        "",
        "1. **Regression gate — candidate vs pinned mssql-odbc baseline.** "
        "This compares the current Rust driver with the pinned older Rust driver "
        "and can fail the run.",
    ]
    if reference_paths:
        lines.append(
            f"2. **Reference gap — candidate vs {reference_label}.** This shows "
            "the performance gap to the production Microsoft driver. "
            "**Informational means it does not fail this regression run; it does "
            "not mean the difference is insignificant.**"
        )
    lines.extend(["", f"**Regression-gate verdict: {verdict}**", ""])
    if reference_paths:
        lines.extend(
            [
                "### Median complete-result wall time",
                "",
                (
                    f"| Benchmark | Pinned mssql-odbc baseline | "
                    f"{reference_label} (non-gating reference) | Candidate | "
                    "Candidate rows/s | "
                    "Candidate cells/s |"
                ),
                "|---|---:|---:|---:|---:|---:|",
            ]
        )
        for result in results:
            lines.append(
                "| `{name}` | {base:.2f} ms | {reference:.2f} ms | "
                "{candidate:.2f} ms | {rows} | {cells} |".format(
                    name=result["name"],
                    base=result["baseline_median_seconds"] * 1000,
                    reference=result["reference_median_seconds"] * 1000,
                    candidate=result["candidate_median_seconds"] * 1000,
                    rows=format_rate(result["rows_per_second"]),
                    cells=format_rate(result["cells_per_second"]),
                )
            )
        lines.extend(
            [
                "",
                "_Medians measured in the initial pass. Re-measured benchmarks "
                "carry their confirmation median into the chart below._",
            ]
        )
        lines.extend(
            [
                "",
                "### Candidate vs pinned mssql-odbc baseline",
                "",
                (
                    "_🟩 lower wall time · 🟥 higher wall time · "
                    f"1 square ≈ {BAR_PERCENT_PER_SQUARE:.0f} percentage points · "
                    f"capped at {BAR_MAX_SQUARES} squares · ⟳ re-measured (median "
                    "of the confirmation rounds). Speedup factor is baseline wall "
                    "time / candidate wall time._"
                ),
                "",
            ]
        )
        lines.extend(
            diverging_bar_table(
                results, "change_percent", "candidate_ratio", mark_confirmed=True
            )
        )
        lines.extend(
            [
                "",
                f"### Reference gap: candidate vs {reference_label} (non-gating)",
                "",
                (
                    "_🟩 lower wall time · 🟥 higher wall time · "
                    f"1 square ≈ {BAR_PERCENT_PER_SQUARE:.0f} percentage points · "
                    f"capped at {BAR_MAX_SQUARES} squares. Speedup factor is "
                    f"{reference_label} wall time / candidate wall time._"
                ),
                "",
            ]
        )
        lines.append(
            "**How to read this table:** red means the candidate took longer than "
            "the Microsoft driver—not that it regressed from the pinned Rust "
            "baseline. For this run:"
        )
        for result in results:
            lines.append(
                f"- `{result['name']}`: candidate took "
                f"**{format_time_multiple(result['candidate_vs_reference_ratio'])}** "
                f"({format_wall_time_change(result['candidate_vs_reference_change_percent'])})."
            )
        lines.append("")
        lines.extend(
            diverging_bar_table(
                results,
                "candidate_vs_reference_change_percent",
                "candidate_vs_reference_ratio",
            )
        )
    else:
        lines.extend(
            [
                "| Benchmark | Baseline median | Candidate median | Initial change | Headline change | Speedup factor | Candidate rows/s | Candidate cells/s |",
                "|---|---:|---:|---|---|---:|---:|---:|",
            ]
        )
        for result in results:
            mark = ""
            if "confirmation_ratio" in result:
                if result.get("confirmed_regression"):
                    status = "confirmed regression"
                    hits = result["confirmation_regression_hits"]
                elif result.get("confirmed"):
                    status = "verified improvement"
                    hits = result["confirmation_hits"]
                else:
                    status = "not confirmed"
                    hits = result["confirmation_hits"]
                mark = (
                    f" ⟳ ({hits}/"
                    f"{result['confirmation_runs']}; {status})"
                )
            lines.append(
                "| `{name}`{mark} | {base:.2f} ms | {candidate:.2f} ms | "
                "{initial_change} | {headline_change} | {speedup} | {rows} | "
                "{cells} |".format(
                    name=result["name"],
                    mark=mark,
                    base=result["baseline_median_seconds"] * 1000,
                    candidate=result["candidate_median_seconds"] * 1000,
                    initial_change=format_wall_time_change(
                        result["initial_change_percent"]
                    ),
                    headline_change=format_wall_time_change(result["change_percent"]),
                    speedup=format_speedup(result["candidate_ratio"]),
                    rows=format_rate(result["rows_per_second"]),
                    cells=format_rate(result["cells_per_second"]),
                )
            )
    lines.extend(
        confirmation_section(
            results,
            threshold,
            improvement_ratio,
            repetitions,
            confirm_runs,
            confirm_quorum,
            improvements_skipped,
        )
    )
    if paired:
        lines.extend(
            paired_section(paired, threshold, reference_label, paired_delay_ns)
        )
    if repetitions is None:
        sample_counts = {result["candidate_samples"] for result in results}
        repetitions = sample_counts.pop() if len(sample_counts) == 1 else None
    lines.extend(
        [
            "",
            "### Method and artifacts",
            "",
        ]
    )
    if repetitions is not None:
        lines.append(
            f"Each driver/workload was measured with **{repetitions} repetitions**; "
            "the tables compare the median complete-result wall time."
        )
    else:
        lines.append(
            "The tables compare the median complete-result wall time from the raw "
            "iteration samples."
        )
    if official_compare:
        lines.append("")
        lines.append(
            "Ratios and medians come from Google Benchmark's own "
            "`tools/compare.py`; its pairwise text and JSON output is retained "
            "alongside this report. The Mann-Whitney U test is disabled because "
            "the run uses fewer repetitions than the nine Google Benchmark "
            "documents as meaningful for it."
        )
    lines.extend(
        [
            "",
            f"- Pinned mssql-odbc baseline commit: `{baseline_commit or 'not provided'}`",
            f"- Baseline samples: {describe_inputs(baseline_paths)}",
            f"- Candidate samples: {describe_inputs(candidate_paths)}",
        ]
    )
    if any(result["name"].startswith("write/") for result in results):
        lines.append(
            "- Write workload modes: candidate and Microsoft use one parameter-array "
            "`SQLExecute`; the pinned Rust baseline uses 2,000 sequential prepared "
            "`SQLExecute` calls because it predates parameter-array support."
        )
    if reference_paths:
        lines.extend(
            [
                f"- Microsoft ODBC Driver version: `{reference_version or reference_label}`",
                f"- Microsoft samples: {describe_inputs(reference_paths)}",
            ]
        )
    if paired_path is not None:
        lines.append(f"- Paired latency rounds: {describe_inputs([paired_path])}")
    lines.append("")
    if reference_paths:
        lines.append(
            f"Only candidate vs pinned mssql-odbc baseline controls the "
            f"{(threshold - 1.0) * 100:.0f}% regression gate, and only after "
            f"confirmation. The {reference_label} comparison quantifies the "
            "non-gating reference gap."
        )
    else:
        lines.append(
            "The gate is based on median complete-result wall time. Throughput "
            "counters come from the candidate samples."
        )
    if paired:
        lines.append("")
        lines.append(
            "Paired latency entries are the exception: they gate on the paired "
            "rule stated in their section, without confirmation rounds."
        )
    return "\n".join(add_section_spacing(lines)) + "\n"


def parse_args():
    """Define additive policy flags without changing the legacy two-way contract."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", required=True, type=Path, action="append")
    parser.add_argument("--candidate", required=True, type=Path, action="append")
    parser.add_argument("--reference", type=Path, action="append")
    parser.add_argument("--reference-label", default="Reference")
    parser.add_argument("--baseline-commit")
    parser.add_argument("--reference-version")
    parser.add_argument("--repetitions", type=int)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument(
        "--regression-ratio", type=float, default=DEFAULT_REGRESSION_RATIO
    )
    parser.add_argument("--improvement-ratio", type=float)
    parser.add_argument("--improvement-max", type=int, default=DEFAULT_IMPROVEMENT_MAX)
    parser.add_argument("--gbench-compare", type=Path)
    # Raw rounds from mssql_odbc_bench_paired. Its entries carry their own verdict,
    # so the runners pass it only to the final, gating invocation.
    parser.add_argument("--paired", type=Path)
    parser.add_argument("--plan-out", type=Path)
    parser.add_argument("--ratios-out", type=Path)
    # The perf template attaches Markdown from the results tree, so the screening
    # and per-round invocations skip summary.md and leave one report to attach.
    parser.add_argument("--no-summary", action="store_true")
    parser.add_argument("--confirm-runs", type=int)
    parser.add_argument("--confirm-quorum", type=int)
    parser.add_argument(
        "--confirmation",
        nargs=4,
        action="append",
        metavar=("NAME", "DIRECTION_HITS", "REGRESSION_HITS", "RATIO"),
        default=[],
    )
    # The gate is on by default; --fail-on-regression stays accepted so existing
    # callers keep working, and --no-fail-on-regression is the way to opt out.
    parser.add_argument("--fail-on-regression", action="store_true")
    parser.add_argument("--no-fail-on-regression", action="store_true")
    return parser.parse_args()


def parse_confirmations(raw):
    """Turn repeated direction/regression hit counts and ratios into a lookup."""
    confirmations = {}
    for name, hits, regression_hits, ratio in raw:
        if name in confirmations:
            raise ValueError(f"duplicate confirmation result for '{name}'")
        try:
            parsed_hits = int(hits)
            parsed_regression_hits = int(regression_hits)
            parsed_ratio = float(ratio)
        except ValueError:
            raise ValueError(
                f"confirmation for '{name}' needs integer direction/regression "
                f"hit counts and a numeric ratio (got '{hits}', "
                f"'{regression_hits}', '{ratio}')"
            ) from None
        if parsed_hits < 0 or parsed_regression_hits < 0:
            raise ValueError(f"confirmation hits for '{name}' must not be negative")
        if not math.isfinite(parsed_ratio) or parsed_ratio <= 0:
            raise ValueError(f"confirmation ratio for '{name}' must be positive")
        confirmations[name] = {
            "hits": parsed_hits,
            "regression_hits": parsed_regression_hits,
            "ratio": parsed_ratio,
        }
    return confirmations


def validate_args(args):
    """Reject settings that would silently publish an unusable verdict."""
    if args.fail_on_regression and args.no_fail_on_regression:
        raise ValueError(
            "--fail-on-regression and --no-fail-on-regression are mutually exclusive"
        )
    if not math.isfinite(args.regression_ratio) or args.regression_ratio <= 1.0:
        raise ValueError("--regression-ratio must be a finite value greater than 1")
    if args.improvement_ratio is not None and (
        not math.isfinite(args.improvement_ratio) or args.improvement_ratio <= 1.0
    ):
        raise ValueError("--improvement-ratio must be a finite value greater than 1")
    if args.improvement_max < 0:
        raise ValueError("--improvement-max must not be negative")
    if args.reference and not args.reference_label.strip():
        raise ValueError("--reference-label must not be empty")
    if args.repetitions is not None and args.repetitions < 1:
        raise ValueError("--repetitions must be a positive integer")
    if args.confirm_runs is not None and args.confirm_runs < 1:
        raise ValueError("--confirm-runs must be a positive integer")
    if args.confirmation and args.confirm_runs is None:
        raise ValueError("--confirmation requires --confirm-runs")
    if args.confirm_runs is not None:
        if args.confirm_quorum is None:
            args.confirm_quorum = args.confirm_runs // 2 + 1
        if not 1 <= args.confirm_quorum <= args.confirm_runs:
            raise ValueError("--confirm-quorum must be between 1 and --confirm-runs")
    elif args.confirm_quorum is not None:
        raise ValueError("--confirm-quorum requires --confirm-runs")


def official_pairwise(args, output_dir):
    """Delegate both pairwise comparisons to Google Benchmark's own tool."""
    if args.gbench_compare is None:
        return None, None
    if not args.gbench_compare.is_file():
        raise ValueError(f"--gbench-compare not found: {args.gbench_compare}")
    merged_baseline = merge_scenario_documents(
        args.baseline, output_dir / "gbench-merged-baseline.json"
    )
    merged_candidate = merge_scenario_documents(
        args.candidate, output_dir / "gbench-merged-candidate.json"
    )
    baseline_medians = official_medians(
        run_official_compare(
            args.gbench_compare,
            merged_baseline,
            merged_candidate,
            output_dir,
            "baseline-vs-candidate",
        ),
        "baseline vs candidate",
    )
    reference_medians = None
    if args.reference:
        merged_reference = merge_scenario_documents(
            args.reference, output_dir / "gbench-merged-reference.json"
        )
        reference_medians = official_medians(
            run_official_compare(
                args.gbench_compare,
                merged_reference,
                merged_candidate,
                output_dir,
                "reference-vs-candidate",
            ),
            "reference vs candidate",
        )
    return baseline_medians, reference_medians


def main():
    """Write log, Markdown, and JSON artifacts from one validated comparison."""
    args = parse_args()
    validate_args(args)
    confirmations = parse_confirmations(args.confirmation)
    if args.confirm_runs is not None:
        excessive_hits = sorted(
            name
            for name, confirmation in confirmations.items()
            if confirmation["hits"] > args.confirm_runs
            or confirmation["regression_hits"] > args.confirm_runs
        )
        if excessive_hits:
            raise ValueError(
                f"confirmation hits exceed --confirm-runs for {excessive_hits}"
            )

    args.output_dir.mkdir(parents=True, exist_ok=True)
    baseline_medians, reference_medians = official_pairwise(args, args.output_dir)

    baseline = load_samples(args.baseline)
    candidate = load_samples(args.candidate)
    reference = load_samples(args.reference) if args.reference else None
    results = compare(
        baseline,
        candidate,
        args.regression_ratio,
        reference,
        baseline_medians,
        reference_medians,
        args.improvement_ratio,
        confirmations,
        args.confirm_runs,
        args.confirm_quorum,
    )
    plan, improvements_skipped = confirmation_plan(results, args.improvement_max)
    improvement_ratio = args.improvement_ratio or args.regression_ratio
    paired = []
    paired_delay_ns = 0
    if args.paired is not None:
        paired_document = load_paired(args.paired, set(baseline))
        paired = paired_results(paired_document, args.regression_ratio, improvement_ratio)
        paired_delay_ns = paired_document["candidate_delay_ns"]

    reference_label = args.reference_label.strip() if reference else None
    text = comparison_text(results, reference_label)
    if paired:
        text += paired_text(paired)
    (args.output_dir / "comparison.txt").write_text(text, encoding="utf-8")
    if not args.no_summary:
        (args.output_dir / "summary.md").write_text(
            summary_markdown(
                results,
                args.regression_ratio,
                improvement_ratio,
                args.baseline,
                args.candidate,
                args.reference,
                reference_label,
                args.baseline_commit,
                args.reference_version,
                args.repetitions,
                args.confirm_runs,
                args.confirm_quorum,
                improvements_skipped,
                args.gbench_compare is not None,
                paired,
                args.paired,
                paired_delay_ns,
            ),
            encoding="utf-8",
        )
    output = {
        "regression_ratio": args.regression_ratio,
        "improvement_ratio": improvement_ratio,
        "improvements_skipped": improvements_skipped,
        "benchmarks": results + paired,
    }
    if paired:
        output["paired_candidate_delay_ns"] = paired_delay_ns
    if reference_label is not None:
        output["reference_label"] = reference_label
    if args.baseline_commit is not None:
        output["baseline_commit"] = args.baseline_commit
    if args.reference_version is not None:
        output["reference_version"] = args.reference_version
    if args.repetitions is not None:
        output["repetitions"] = args.repetitions
    if args.confirm_runs is not None:
        output["confirm_runs"] = args.confirm_runs
        output["confirm_quorum"] = args.confirm_quorum
    (args.output_dir / "comparison.json").write_text(
        json.dumps(output, indent=2) + "\n",
        encoding="utf-8",
    )
    if args.plan_out is not None:
        args.plan_out.parent.mkdir(parents=True, exist_ok=True)
        args.plan_out.write_text(
            "".join(f"{kind}\t{name}\t{ratio:.6f}\n" for kind, name, ratio in plan),
            encoding="utf-8",
        )
    if args.ratios_out is not None:
        args.ratios_out.parent.mkdir(parents=True, exist_ok=True)
        args.ratios_out.write_text(
            "".join(
                f"{result['name']}\t{result['initial_ratio']:.6f}\n"
                for result in results
            ),
            encoding="utf-8",
        )
    sys.stdout.write(text)

    gate_enabled = not args.no_fail_on_regression
    if gate_enabled and any(result["regression"] for result in results + paired):
        return 2
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)

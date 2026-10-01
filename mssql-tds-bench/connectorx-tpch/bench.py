# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
"""A/B harness for ConnectorX `read_sql` over TPC-H lineitem on SQL Server.

`run` orchestrates; `child` performs one timed read in a fresh interpreter so
every sample starts from the same allocator and connection-pool state and peak
RSS is attributable to that single read. Each arm is a (python, driver) pair, so
two ConnectorX releases live in separate venvs and the runtime driver switch in
the newer release isolates the driver from the rest of the release.
"""

import argparse
import json
import os
import random
import statistics
import subprocess
import sys
import time
from urllib.parse import quote_plus

CONN_ENV = "CX_BENCH_CONN"
IDENT_SQL = (
    "SELECT s.client_interface_name, s.program_name, c.encrypt_option, c.net_packet_size "
    "FROM sys.dm_exec_sessions s JOIN sys.dm_exec_connections c ON c.session_id = s.session_id "
    "WHERE s.session_id = @@SPID"
)
# (baseline, candidate, title). Times are compared as candidate / baseline.
COMPARISONS = [
    ("before", "now", "Before (Tiberius release) → now (mssql-tds default)"),
    ("now-tiberius", "now", "Driver only: same release, Tiberius → mssql-tds"),
    ("before", "now-tiberius", "Control: release change only, Tiberius in both"),
]


def child(args):
    from importlib.metadata import version
    import resource

    import connectorx as cx

    if args.driver:
        cx.mssql_driver = args.driver
    conn = os.environ[CONN_ENV]
    kwargs = {"return_type": args.return_type}
    if args.partition_num > 1:
        kwargs.update(partition_on=args.partition_on, partition_num=args.partition_num)

    start = time.perf_counter()
    result = cx.read_sql(conn, args.query, **kwargs)
    seconds = time.perf_counter() - start
    rows = result.num_rows if args.return_type == "arrow" else len(result)
    del result

    out = {
        "seconds": seconds,
        "rows": rows,
        "peak_rss_mb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024,
        "connectorx": version("connectorx"),
        # Releases before the runtime switch only ship Tiberius.
        "driver": getattr(cx, "mssql_driver", "tiberius (only driver)"),
    }
    if args.identify:
        out["identity"] = cx.read_sql(conn, IDENT_SQL, return_type="arrow").to_pylist()[0]
    print(json.dumps(out))


def parse_arm(spec):
    parts = spec.split("|")
    if len(parts) not in (2, 3) or not parts[0] or not parts[1]:
        raise argparse.ArgumentTypeError(f"arm must be name|python[|driver], got {spec!r}")
    return {"name": parts[0], "python": parts[1], "driver": parts[2] if len(parts) == 3 else ""}


def csv_list(value):
    return [v.strip() for v in value.split(",") if v.strip()]


def run_child(args, arm, scenario, conn, identify):
    cmd = list(args.prefix) + [
        arm["python"], os.path.abspath(__file__), "child",
        "--query", args.query,
        "--partition-on", args.partition_on,
        "--partition-num", str(scenario["partitions"]),
        "--return-type", scenario["return_type"],
    ]
    if arm["driver"]:
        cmd += ["--driver", arm["driver"]]
    if identify:
        cmd.append("--identify")
    # The connection string carries the password, so keep it out of argv.
    env = dict(os.environ, **{CONN_ENV: conn})
    try:
        proc = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=args.child_timeout)
    except subprocess.TimeoutExpired:
        return {"error": f"timed out after {args.child_timeout}s"}
    if proc.returncode != 0:
        return {"error": (proc.stderr or proc.stdout).strip()[-2000:]}
    return json.loads(proc.stdout.strip().splitlines()[-1])


def bootstrap_ratio_ci(base, cand, rng, resamples=4000):
    ratios = sorted(
        statistics.median(rng.choices(cand, k=len(cand))) / statistics.median(rng.choices(base, k=len(base)))
        for _ in range(resamples)
    )
    return ratios[int(0.025 * resamples)], ratios[int(0.975 * resamples) - 1]


def summarize(samples):
    secs = [s["seconds"] for s in samples]
    rss = [s["peak_rss_mb"] for s in samples]
    return {
        "n": len(secs),
        "median": statistics.median(secs),
        "min": min(secs),
        "max": max(secs),
        "cv_pct": (statistics.stdev(secs) / statistics.mean(secs) * 100) if len(secs) > 1 else 0.0,
        "peak_rss_mb": statistics.median(rss),
        "seconds": secs,
    }


def scenario_key(s):
    return f"encrypt={s['encrypt']} partitions={s['partitions']} {s['return_type']}"


def run(args):
    arms = args.arm
    names = [a["name"] for a in arms]
    if len(set(names)) != len(names):
        sys.exit("arm names must be unique")
    os.makedirs(args.results_dir, exist_ok=True)
    rng = random.Random(args.seed)
    password = quote_plus(os.environ["SQL_PASSWORD"])
    scenarios = [
        {"encrypt": e, "partitions": p, "return_type": r}
        for e in args.encrypt_modes
        for p in (int(x) for x in args.partitions)
        for r in args.return_types
    ]

    identities, failures, results = {}, [], {}
    with open(os.path.join(args.results_dir, "raw.jsonl"), "w", encoding="utf-8") as raw:
        for scenario in scenarios:
            key = scenario_key(scenario)
            conn = (
                f"mssql://{args.user}:{password}@{args.host}:{args.port}/{args.database}"
                f"?encrypt={scenario['encrypt']}&trust_server_certificate=true"
            )
            samples = {n: [] for n in names}
            failed = set()
            # Arms are interleaved within each round (shuffled order) so slow drift
            # in the host or SQL Server hits every arm equally.
            for rnd in range(args.warmup_rounds + args.rounds):
                order = arms[:]
                rng.shuffle(order)
                warm = rnd < args.warmup_rounds
                for arm in order:
                    if arm["name"] in failed:
                        continue
                    res = run_child(args, arm, scenario, conn, identify=(rnd == 0))
                    raw.write(json.dumps({"scenario": key, "arm": arm["name"], "round": rnd, "warmup": warm, **res}) + "\n")
                    raw.flush()
                    if "error" in res:
                        failed.add(arm["name"])
                        failures.append(f"{key} / {arm['name']}: {res['error'].splitlines()[-1]}")
                        print(f"!!! [{key}] {arm['name']} FAILED: {res['error']}", flush=True)
                        continue
                    if args.expected_rows and res["rows"] != args.expected_rows:
                        failed.add(arm["name"])
                        failures.append(f"{key} / {arm['name']}: {res['rows']} rows, expected {args.expected_rows}")
                        continue
                    if "identity" in res:
                        identities[(key, arm["name"])] = {
                            **res["identity"], "driver": res["driver"], "connectorx": res["connectorx"]
                        }
                    tag = "warm-up" if warm else f"round {rnd - args.warmup_rounds + 1}/{args.rounds}"
                    print(f">>> [{key}] {tag} {arm['name']}: {res['seconds']:.2f}s "
                          f"rss={res['peak_rss_mb']:.0f}MB", flush=True)
                    if not warm:
                        samples[arm["name"]].append(res)
            results[key] = {n: summarize(s) for n, s in samples.items() if s}

    check_identities(identities, failures)
    report = build_report(args, scenarios, results, identities, failures, rng)
    with open(os.path.join(args.results_dir, "comparison.json"), "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2)
    with open(os.path.join(args.results_dir, "summary.md"), "w", encoding="utf-8") as f:
        f.write(render_markdown(report))
    if failures:
        sys.exit(1)


def check_identities(identities, failures):
    # The runtime switch must change what goes on the wire; otherwise the
    # "driver only" comparison measures Tiberius against itself.
    by_scenario = {}
    for (key, arm), ident in identities.items():
        by_scenario.setdefault(key, {})[arm] = ident.get("client_interface_name")
    for key, arms in by_scenario.items():
        tib, tds = arms.get("now-tiberius"), arms.get("now")
        if tib is not None and tds is not None and tib == tds:
            failures.append(f"{key}: 'now' and 'now-tiberius' report the same client_interface_name ({tib!r})")


def build_report(args, scenarios, results, identities, failures, rng):
    comparisons = []
    for scenario in scenarios:
        key = scenario_key(scenario)
        stats = results.get(key, {})
        for base, cand, title in COMPARISONS:
            if base not in stats or cand not in stats:
                continue
            b, c = stats[base], stats[cand]
            ratio = c["median"] / b["median"]
            lo, hi = bootstrap_ratio_ci(b["seconds"], c["seconds"], rng)
            comparisons.append({
                "scenario": key, "baseline": base, "candidate": cand, "title": title,
                "baseline_median_s": b["median"], "candidate_median_s": c["median"],
                "time_ratio": ratio, "speedup": 1 / ratio, "ci95": [lo, hi],
                "significant": hi < 1 or lo > 1,
                "rss_ratio": c["peak_rss_mb"] / b["peak_rss_mb"],
            })
    return {
        "config": {
            "scale_factor": args.scale_factor, "rounds": args.rounds, "warmup_rounds": args.warmup_rounds,
            "query": args.query, "expected_rows": args.expected_rows, "seed": args.seed,
            "arms": [{k: a[k] for k in ("name", "driver")} for a in args.arm],
        },
        "identities": [{"scenario": k, "arm": a, **v} for (k, a), v in identities.items()],
        "results": results,
        "comparisons": comparisons,
        "failures": failures,
    }


def render_markdown(report):
    cfg = report["config"]
    out = [
        f"## ConnectorX TPC-H lineitem SF{cfg['scale_factor']} — Tiberius vs mssql-tds",
        "",
        f"`{cfg['query']}` · {cfg['rounds']} measured rounds (+{cfg['warmup_rounds']} warm-up) per arm, "
        "arm order shuffled every round, one fresh process per read. Speedup > 1 means the candidate "
        "is faster. 95% CI is a bootstrap of the median-time ratio (candidate / baseline); "
        "✅/❌ marks results whose CI excludes 1.0.",
        "",
    ]
    if report["failures"]:
        out += ["### ⚠️ Failures", ""] + [f"- {f}" for f in report["failures"]] + [""]

    for base, cand, title in COMPARISONS:
        rows = [c for c in report["comparisons"] if c["baseline"] == base and c["candidate"] == cand]
        if not rows:
            continue
        out += [f"### {title}", "",
                f"| Scenario | {base} median (s) | {cand} median (s) | Speedup | Δ time | 95% CI | Peak RSS ratio |",
                "|---|--:|--:|--:|--:|:--:|--:|"]
        for c in rows:
            mark = ("✅ " if c["time_ratio"] < 1 else "❌ ") if c["significant"] else ""
            out.append(
                f"| {c['scenario']} | {c['baseline_median_s']:.2f} | {c['candidate_median_s']:.2f} | "
                f"{mark}{c['speedup']:.2f}× | {(c['time_ratio'] - 1) * 100:+.1f}% | "
                f"{c['ci95'][0]:.3f} – {c['ci95'][1]:.3f} | {c['rss_ratio']:.2f} |"
            )
        out.append("")

    out += ["### Per-arm detail", "",
            "| Scenario | Arm | n | Median (s) | Min (s) | Max (s) | CV | Rows/s (median) | Peak RSS (MB) |",
            "|---|---|--:|--:|--:|--:|--:|--:|--:|"]
    for key, arms in report["results"].items():
        for name, s in arms.items():
            rps = f"{cfg['expected_rows'] / s['median']:,.0f}" if cfg["expected_rows"] else "—"
            out.append(f"| {key} | {name} | {s['n']} | {s['median']:.2f} | {s['min']:.2f} | {s['max']:.2f} | "
                       f"{s['cv_pct']:.1f}% | {rps} | {s['peak_rss_mb']:.0f} |")
    out.append("")

    out += ["### Arms as observed by SQL Server", "",
            "| Scenario | Arm | connectorx | driver setting | client_interface_name | encrypt_option | packet size |",
            "|---|---|---|---|---|---|--:|"]
    for i in report["identities"]:
        out.append(f"| {i['scenario']} | {i['arm']} | {i['connectorx']} | {i['driver']} | "
                   f"{i.get('client_interface_name')} | {i.get('encrypt_option')} | {i.get('net_packet_size')} |")
    out.append("")
    return "\n".join(out)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)

    c = sub.add_parser("child")
    c.add_argument("--query", required=True)
    c.add_argument("--partition-on", required=True)
    c.add_argument("--partition-num", type=int, default=1)
    c.add_argument("--return-type", default="arrow")
    c.add_argument("--driver", default="")
    c.add_argument("--identify", action="store_true")

    r = sub.add_parser("run")
    r.add_argument("--arm", type=parse_arm, action="append", required=True)
    r.add_argument("--host", required=True)
    r.add_argument("--port", default="1433")
    r.add_argument("--user", default="sa")
    r.add_argument("--database", default="tpch")
    r.add_argument("--query", default="SELECT * FROM lineitem")
    r.add_argument("--partition-on", default="l_orderkey")
    r.add_argument("--partitions", type=csv_list, default=["1", "4"])
    r.add_argument("--encrypt-modes", type=csv_list, default=["false", "true"])
    r.add_argument("--return-types", type=csv_list, default=["arrow"])
    r.add_argument("--rounds", type=int, default=5)
    r.add_argument("--warmup-rounds", type=int, default=1)
    r.add_argument("--expected-rows", type=int, default=0)
    r.add_argument("--scale-factor", default="")
    r.add_argument("--seed", type=int, default=20261001)
    r.add_argument("--child-timeout", type=int, default=1800)
    r.add_argument("--results-dir", required=True)
    # Anything after `--` prefixes each child command (e.g. taskset -c 16-31).
    r.add_argument("prefix", nargs=argparse.REMAINDER)

    args = p.parse_args()
    if args.cmd == "child":
        child(args)
        return
    if args.prefix[:1] == ["--"]:
        args.prefix = args.prefix[1:]
    if args.rounds < 2:
        p.error("--rounds must be >= 2")
    run(args)


if __name__ == "__main__":
    main()

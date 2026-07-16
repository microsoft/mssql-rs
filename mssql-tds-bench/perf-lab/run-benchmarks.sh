#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# run-benchmarks.sh — perf-lab testScript for mssql-tds (Linux).
#
# Runs on the dedicated perf VM. The shared Perf.Test.Job template SCPs the
# repository (this file's repo root) to ~/perf-tests and launches this script
# there, with SQL_SERVER and SQL_PASSWORD injected as environment variables.
#
# It builds the mssql-tds-bench harness TWICE from the SAME (candidate) working
# tree — so the harness code, Criterion version, and toolchain are identical —
# swapping ONLY the mssql-tds source:
#   * candidate: ../mssql-tds is the working tree
#   * baseline:  ../mssql-tds is replaced with a local `git worktree` checkout of
#                the commit pinned in baseline-commit.txt (no ADO auth on the VM)
# Any statistically significant delta is therefore attributable to mssql-tds.
set -euo pipefail

REPO_ROOT="$(pwd)"
RESULTS_DIR="$REPO_ROOT/results"
# Baseline pointer — a committed commit SHA. Advancing the baseline requires a
# PR that edits this file, so every move is reviewed and recorded in history.
BASELINE_FILE="$REPO_ROOT/mssql-tds-bench/perf-lab/baseline-commit.txt"
mkdir -p "$RESULTS_DIR"

# CPU telemetry: bracketed average-frequency/temperature samples written around
# each measured pass so we can validate whether CPU frequency or thermals differ
# between the candidate and baseline passes (the Linux control for the Windows
# noise investigation). Temperature is best-effort (often unavailable in a VM).
TELEMETRY_CSV="$RESULTS_DIR/cpu-telemetry.csv"
echo "timestamp,label,avg_cur_freq_mhz,temp_c" > "$TELEMETRY_CSV"
cpu_sample() {
    local label="$1" sum=0 n=0 f v freq_mhz="" temp_c="" t tv
    for f in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_cur_freq; do
        [ -r "$f" ] || continue
        v=$(cat "$f" 2>/dev/null) || continue
        sum=$(( sum + v )); n=$(( n + 1 ))
    done
    if [ "$n" -gt 0 ]; then freq_mhz=$(( sum / n / 1000 )); fi
    for t in /sys/class/thermal/thermal_zone*/temp; do
        [ -r "$t" ] || continue
        tv=$(cat "$t" 2>/dev/null) || continue
        temp_c=$(( tv / 1000 )); break
    done
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ),${label},${freq_mhz},${temp_c}" >> "$TELEMETRY_CSV"
    echo ">>> cpu[${label}] avgFreq=${freq_mhz}MHz temp=${temp_c}C"
}

# --- Connection (SQL_SERVER / SQL_PASSWORD injected by run-remote.sh) ---
export DB_HOST="${SQL_SERVER:?SQL_SERVER not set}"
export DB_PORT="${DB_PORT:-1433}"
export DB_USERNAME="${DB_USERNAME:-sa}"
export TRUST_SERVER_CERTIFICATE="${TRUST_SERVER_CERTIFICATE:-true}"
# SQL_PASSWORD is already exported into this session by run-remote.sh.
: "${SQL_PASSWORD:?SQL_PASSWORD not set}"

# --- SQL Server configuration snapshot (validate the instance is tuned) ---
# Dump effective memory / MAXDOP / cost-threshold / affinity, tempdb placement,
# durability/recovery, and trace flags so we can confirm the perf tuning took.
# Best-effort - never fail the run over it (sqlcmd may be absent on the client).
SQL_CONFIG_SQL="$REPO_ROOT/mssql-tds-bench/perf-lab/sql-config-dump.sql"
sqlcmd_bin="$(command -v sqlcmd || true)"
if [ -z "$sqlcmd_bin" ] && [ -x /opt/mssql-tools18/bin/sqlcmd ]; then sqlcmd_bin=/opt/mssql-tools18/bin/sqlcmd; fi
if [ -z "$sqlcmd_bin" ] && [ -x /opt/mssql-tools/bin/sqlcmd ]; then sqlcmd_bin=/opt/mssql-tools/bin/sqlcmd; fi
if [ -n "$sqlcmd_bin" ] && [ -f "$SQL_CONFIG_SQL" ]; then
    echo ">>> Capturing SQL Server configuration snapshot..."
    "$sqlcmd_bin" -S "$SQL_SERVER" -U "$DB_USERNAME" -P "$SQL_PASSWORD" -C -b -y 0 -Y 30 -i "$SQL_CONFIG_SQL" \
        | tee "$RESULTS_DIR/sql-config.txt" || echo ">>> SQL config snapshot skipped (query failed)."
else
    echo ">>> Skipping SQL config snapshot (sqlcmd or query file not found)."
fi

# --- System prerequisites (Ubuntu) ---
# The perf VM may be a minimal image without git or a C toolchain. Install what
# the run needs up front: git (for the baseline worktree), curl (for rustup),
# and a C linker (to compile the benches).
ensure_packages() {
    local missing=()
    command -v git >/dev/null 2>&1 || missing+=(git)
    command -v curl >/dev/null 2>&1 || missing+=(curl)
    command -v cc >/dev/null 2>&1 || missing+=(build-essential)
    command -v pkg-config >/dev/null 2>&1 || missing+=(pkg-config)
    [ -f /usr/include/openssl/ssl.h ] || missing+=(libssl-dev)
    [ -f /etc/ssl/certs/ca-certificates.crt ] || missing+=(ca-certificates)
    [ ${#missing[@]} -eq 0 ] && return 0

    local sudo=""
    [ "$(id -u)" -ne 0 ] && sudo="sudo"
    echo ">>> Installing system packages: ${missing[*]}"
    $sudo apt-get update -y
    $sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${missing[@]}"
}
ensure_packages

# --- Toolchain ---
if ! command -v cargo >/dev/null 2>&1; then
    echo ">>> Installing Rust toolchain via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export PATH="$HOME/.cargo/bin:$PATH"

if ! command -v critcmp >/dev/null 2>&1; then
    echo ">>> Installing critcmp..."
    cargo install critcmp --version 0.1.8 --locked
fi

# --- Kernel network tuning for high connection churn ---
# The concurrent_connects benchmark opens tens of thousands of short-lived TCP
# connections. On a default Ubuntu image the ephemeral port range plus TIME_WAIT
# accumulation exhausts local ports, so connect() fails with EADDRNOTAVAIL
# (errno 99, "Cannot assign requested address"). Widen the port range and allow
# reusing TIME_WAIT sockets for new outbound connections. Best-effort: skip
# quietly if sysctl is unavailable or not permitted.
tune_network() {
    command -v sysctl >/dev/null 2>&1 || return 0
    local sudo=""
    [ "$(id -u)" -ne 0 ] && sudo="sudo"
    echo ">>> Tuning ephemeral ports / TIME_WAIT reuse for connection benchmarks..."
    $sudo sysctl -w net.ipv4.ip_local_port_range="1024 65535" || true
    $sudo sysctl -w net.ipv4.tcp_tw_reuse=1 || true
}
tune_network

# --- Resolve and verify the baseline commit (from baseline-commit.txt) ---
if [ ! -f "$BASELINE_FILE" ]; then
    echo "ERROR: baseline file not found: ${BASELINE_FILE}" >&2
    exit 1
fi
BASELINE_COMMIT="$(grep -vE '^[[:space:]]*(#|$)' "$BASELINE_FILE" | head -n1 | tr -d '[:space:]')"
if ! printf '%s' "$BASELINE_COMMIT" | grep -qE '^[0-9a-fA-F]{7,40}$'; then
    echo "ERROR: ${BASELINE_FILE} does not contain a valid commit SHA (got: '${BASELINE_COMMIT}')" >&2
    exit 1
fi
if ! git rev-parse --verify --quiet "${BASELINE_COMMIT}^{commit}" >/dev/null; then
    echo "ERROR: baseline commit '${BASELINE_COMMIT}' not found in the shipped repository." >&2
    echo "       Ensure the pipeline checkout fetches full history (the commit must be present)." >&2
    exit 1
fi
echo ">>> Baseline commit: ${BASELINE_COMMIT}"

# --- Release-grade sampling for the lab ---
# Heavier than the lighter defaults baked into criterion_config() (which keep a
# local `cargo bench` fast). More warm-up lets the SQL plan cache / buffer pool /
# tempdb settle; more measurement time and samples separate small real deltas
# from run-to-run noise. Pre-set any of these to override.
export BENCH_WARMUP_SECS="${BENCH_WARMUP_SECS:-10}"
export BENCH_SECS="${BENCH_SECS:-30}"
export BENCH_SAMPLES="${BENCH_SAMPLES:-30}"

# --- Allocator tuning (steadier large-buffer / LOB benchmarks) ---
# The LOB benches decode multi-MB buffers each iteration. With glibc's defaults a
# 20 MB allocation is served by mmap and returned to the OS on free, so every
# iteration re-mmaps and re-faults the pages (slow and noisy). Raise the mmap
# threshold so those allocations come from the heap (brk), and disable heap
# trimming so the memory is reused instead of handed back. (Setting the mmap
# threshold high is required: setting only the trim threshold would disable
# glibc's dynamic mmap threshold and force every large allocation through mmap.)
export MALLOC_MMAP_THRESHOLD_="${MALLOC_MMAP_THRESHOLD_:-134217728}"  # 128 MB
export MALLOC_TRIM_THRESHOLD_="${MALLOC_TRIM_THRESHOLD_:--1}"          # never trim

# --- Optional CPU pinning (avoid contention with a colocated SQL Server) ---
# When SQL Server runs on the same VM, pin the benchmark client to a core set
# DISJOINT from the one SQL Server is pinned to, so the two do not fight for the
# same CPUs. The perf lab is expected to reserve cores for SQL Server and publish
# the free set via PERF_CLIENT_CPUS (e.g. "16-31"). BENCH_CPUS overrides locally.
# If neither is set, or taskset is unavailable, the benchmarks run unpinned.
BENCH_CPUS="${BENCH_CPUS:-${PERF_CLIENT_CPUS:-}}"
BENCH_PREFIX=()
if [ -n "$BENCH_CPUS" ]; then
    if command -v taskset >/dev/null 2>&1; then
        echo ">>> Pinning benchmark client to CPUs: ${BENCH_CPUS}"
        BENCH_PREFIX=(taskset -c "$BENCH_CPUS")
    else
        echo ">>> taskset unavailable; running unpinned (requested CPUs: ${BENCH_CPUS})"
    fi
fi

# --- Warm-up passes (discarded) ---
# Each measured pass is preceded by a fast, discarded run of the same benchmarks
# to prime SQL Server's buffer pool and the OS page cache so it starts warm.
# This must run before BOTH the candidate and the baseline passes: the candidate
# pass runs for many minutes and the baseline mssql-tds is then rebuilt (which
# churns memory and the page cache), so a single warm-up before the candidate
# does NOT leave the baseline warm. Without a re-warm the second (baseline) pass
# pays a cold-cache penalty that shows up as the baseline looking spuriously
# slower, worst on the I/O-heavy benches (LOB, packet-size). $1 optionally limits
# the warm-up to a Criterion benchmark-id regex.
warmup_pass() {
    echo ">>> Warm-up pass (discarded)${1:+ [$1]}..."
    BENCH_WARMUP_SECS=1 BENCH_SECS=1 BENCH_SAMPLES=10 \
        "${BENCH_PREFIX[@]}" cargo bench -p mssql-tds-bench -- --save-baseline warmup ${1:+"$1"} >/dev/null 2>&1 || true
}

# --- Build both sides, then interleave per bench binary --------------------
# To make each benchmark's candidate and baseline measurements adjacent in time
# (which cancels the slow drift that otherwise makes the second, baseline pass
# look spuriously slower), build BOTH bench binaries up front and run them
# per-binary back-to-back instead of all-candidate-then-all-baseline. Criterion
# writes to $CRITERION_HOME; both sides point at the shared target/criterion so
# critcmp can compare them. The two sides are built into separate target dirs so
# both persist. (Interleaving per bench BINARY, not per individual bench: a
# per-bench filter would still re-run every bench's setup each time, so per-binary
# keeps setup cost — and total run time — the same as the old two-pass approach.)

# Print "<bench-name><TAB><exe-path>" for each built bench binary. $1 = target dir.
bench_bins() {
    CARGO_TARGET_DIR="$1" cargo bench -p mssql-tds-bench --no-run --message-format=json 2>/dev/null \
        | python3 -c 'import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        m = json.loads(line)
    except ValueError:
        continue
    ex = m.get("executable")
    t = m.get("target") or {}
    if ex and "bench" in (t.get("kind") or []):
        print((t.get("name") or "") + "\t" + ex)'
}

swap_to_baseline() {
    mv "$REPO_ROOT/mssql-tds" "$REPO_ROOT/.mssql-tds-candidate"
    cp -r "$BASELINE_TREE/mssql-tds" "$REPO_ROOT/mssql-tds"
}
restore_candidate() {
    rm -rf "$REPO_ROOT/mssql-tds"
    mv "$REPO_ROOT/.mssql-tds-candidate" "$REPO_ROOT/mssql-tds"
}

echo ">>> Building candidate bench binaries (target/)..."
CAND_BINS="$(bench_bins "$REPO_ROOT/target")"
[ -n "$CAND_BINS" ] || { echo "ERROR: no candidate bench binaries found"; exit 1; }

BASELINE_TREE="$(mktemp -d)/perf-baseline"
echo ">>> Adding baseline worktree for ${BASELINE_COMMIT} at ${BASELINE_TREE}..."
git worktree add --detach "$BASELINE_TREE" "$BASELINE_COMMIT"
echo ">>> Building baseline bench binaries (target-base/)..."
swap_to_baseline
BASE_BINS="$(bench_bins "$REPO_ROOT/target-base")"
restore_candidate
git worktree remove --force "$BASELINE_TREE" || true
[ -n "$BASE_BINS" ] || { echo "ERROR: no baseline bench binaries found"; exit 1; }

# Run every bench binary once per side, candidate then baseline back-to-back,
# saving to Criterion baselines $1 (candidate) and $2 (baseline); $3 = optional
# Criterion benchmark-id filter.
interleave_run() {
    local cand_name="$1" base_name="$2" filter="${3:-}"
    export CRITERION_HOME="$REPO_ROOT/target/criterion"
    local bname cpath bpath
    while IFS=$'\t' read -r bname cpath; do
        [ -n "$bname" ] || continue
        bpath=$(printf '%s\n' "$BASE_BINS" | awk -F'\t' -v n="$bname" '$1==n{print $2}')
        [ -n "$bpath" ] || { echo ">>> WARN: no baseline binary for '$bname'; skipping"; continue; }
        echo ">>> [$bname] candidate..."
        "${BENCH_PREFIX[@]}" "$cpath" --bench --save-baseline "$cand_name" ${filter:+"$filter"}
        echo ">>> [$bname] baseline..."
        "${BENCH_PREFIX[@]}" "$bpath" --bench --save-baseline "$base_name" ${filter:+"$filter"}
    done <<< "$CAND_BINS"
    unset CRITERION_HOME
}

# Warm-up once to prime SQL Server's buffer pool and the OS page cache; because
# interleaving keeps each candidate/baseline pair adjacent, one warm-up suffices.
warmup_pass

echo ">>> Interleaving candidate/baseline per bench binary..."
cpu_sample "interleave-start" || true
interleave_run candidate base
cpu_sample "interleave-end" || true

# --- Compare (both baselines live in the shared target/criterion) ---
echo ">>> Comparing base -> candidate..."
critcmp base candidate | tee "$RESULTS_DIR/comparison.txt"

# Print the IDs (field 1) of benchmarks whose candidate ratio (field 6) meets or
# exceeds the regression threshold, one per line.
regression_ids() {
    awk -v thr="${BENCH_REGRESSION_RATIO:-1.10}" '
        $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ && ($6 + 0) >= thr { print $1 }
    ' "$1"
}

OFFENDERS=$(regression_ids "$RESULTS_DIR/comparison.txt")
# The table the gate/verdict act on: the full run by default, or the re-measured
# offenders when auto-confirm runs below.
GATE_TABLE="$RESULTS_DIR/comparison.txt"

# --- Auto-confirm regressions (re-measure only the offenders, interleaved) ---
# A strict gate can trip on a transient single-benchmark outlier. Re-measure ONLY
# the benchmarks that tripped — interleaved per binary, same as the main run — and
# keep as a real regression only those that trip AGAIN. Both binaries are already
# built, so this just replays the offenders (adds only their run time).
if [ -n "$OFFENDERS" ]; then
    FILTER=$(printf '%s\n' "$OFFENDERS" | sed 's|^|^|; s|$|$|' | paste -sd '|' -)
    echo ">>> Gate tripped by: $(printf '%s ' $OFFENDERS)"
    echo ">>> Auto-confirm: re-measuring only those benchmarks (filter: ${FILTER})"
    warmup_pass "$FILTER"
    interleave_run candidate_confirm base_confirm "$FILTER"
    echo ">>> Auto-confirm comparison (base_confirm -> candidate_confirm):"
    critcmp base_confirm candidate_confirm | tee "$RESULTS_DIR/confirm.txt"
    GATE_TABLE="$RESULTS_DIR/confirm.txt"
fi
rm -rf "$REPO_ROOT/target-base" 2>/dev/null || true

# Markdown summary — the perf lab attaches results/*.md to the run's Summary tab
# (task.uploadsummary), so the comparison renders inline on the run page. The
# critcmp table is fixed-width, so wrap it in a fenced code block to keep it
# aligned.
#
# Verdict acts on the gate table (the re-measured offenders after auto-confirm,
# or the full run when nothing tripped): the candidate regressed a bench when its
# ratio (field 6) meets or exceeds the threshold.
VERDICT=$(awk -v thr="${BENCH_REGRESSION_RATIO:-1.10}" '
    $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ {
        cand = $6 + 0
        if (cand >= thr) { n++; if (cand > worst) { worst = cand; wname = $1 } }
    }
    END {
        pct = int((thr - 1) * 100 + 0.5)
        if (n > 0)
            printf "\342\232\240\357\270\217 %d benchmark(s) slower by >=%d%% vs baseline (worst: %s +%d%%)", n, pct, wname, int((worst - 1) * 100 + 0.5)
        else
            printf "\342\234\205 No benchmark slower by >=%d%% vs baseline", pct
    }
' "$GATE_TABLE")

{
    echo "## mssql-tds perf — base → candidate"
    echo ""
    echo "**${VERDICT}**"
    echo ""
    if [ -n "$OFFENDERS" ]; then
        echo "_Auto-confirm re-ran the gate-tripping benchmark(s); the verdict reflects that re-measurement. Benchmarks that tripped once but not on the re-run are treated as transient noise._"
        echo ""
    fi
    echo "Baseline commit: \`${BASELINE_COMMIT}\`"
    echo ""
    echo '```'
    cat "$RESULTS_DIR/comparison.txt"
    echo '```'
    if [ -n "$OFFENDERS" ]; then
        echo ""
        echo "### Auto-confirm re-run (offenders only)"
        echo ""
        echo "Tripped on the first pass: $(printf '%s ' $OFFENDERS)"
        echo ""
        echo '```'
        cat "$RESULTS_DIR/confirm.txt"
        echo '```'
    fi
} > "$RESULTS_DIR/summary.md"

# Archive the raw Criterion data for offline analysis.
cp -r target/criterion "$RESULTS_DIR/criterion" 2>/dev/null || true

echo ">>> Done. Results in ${RESULTS_DIR}"

# Fail the run only on CONFIRMED regressions (the gate table): the full run when
# nothing tripped, or the auto-confirm re-run of the offenders. summary.md and
# the tables above name them.
CONFIRMED=$(regression_ids "$GATE_TABLE" | grep -c . || true)
if [ "${CONFIRMED:-0}" -gt 0 ]; then
    echo ">>> ${VERDICT}"
    echo ">>> FAILING: ${CONFIRMED} benchmark(s) still exceeded the threshold on the auto-confirm re-run (BENCH_REGRESSION_RATIO=${BENCH_REGRESSION_RATIO:-1.10})."
    exit 1
fi
if [ -n "$OFFENDERS" ]; then
    echo ">>> Auto-confirm cleared all $(printf '%s\n' "$OFFENDERS" | grep -c .) initial regression(s) as transient; passing."
fi

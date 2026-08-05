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
# set -e exits silently, which costs a whole lab run to diagnose. Report the
# location and the failing command first. -E inherits the trap into functions,
# subshells, and command substitutions, where most of the risk is. Commands in
# if/&&/||/! conditions do not fire it, and neither does an explicit `exit`, so
# the deliberate failure paths keep their own messages. For a failing pipeline
# the line number is authoritative; BASH_COMMAND reports the last command in it.
set -E
trap 'rc=$?; echo "ERROR: ${BASH_SOURCE[0]}:${LINENO}: \`${BASH_COMMAND}\` exited ${rc}" >&2' ERR

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

# The perf lab always has a server provisioned and injected, so a failure to
# connect must FAIL the run, not skip it. This flag makes the benches' try_connect
# panic instead of returning None (see mssql-tds-bench/src/lib.rs); without it an
# unreachable server would skip every benchmark, leave comparison.txt empty, and
# the gate would pass spuriously green.
export BENCH_REQUIRE_SERVER=1

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
# python3 (to parse cargo's JSON output in bench_bins), and a C linker (to
# compile the benches).
ensure_packages() {
    local missing=()
    command -v git >/dev/null 2>&1 || missing+=(git)
    command -v curl >/dev/null 2>&1 || missing+=(curl)
    command -v python3 >/dev/null 2>&1 || missing+=(python3)
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
# Reuse the repo's canonical rustup installer (the same script the real CI stages
# use, shipped to the VM at .pipeline/scripts/) rather than a second, drifting
# copy. It passes no --default-toolchain, so the repo's rust-toolchain.toml
# (channel = "1.95") drives the version the benches build under.
if ! command -v cargo >/dev/null 2>&1; then
    echo ">>> Installing Rust toolchain via .pipeline/scripts/install-rustup.sh..."
    bash "$REPO_ROOT/.pipeline/scripts/install-rustup.sh"
fi
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export PATH="$HOME/.cargo/bin:$PATH"
# Fail loud if the toolchain still isn't available: the canonical installer does
# not itself abort on a failed download, and the lab must not proceed to a silent
# no-op run.
command -v cargo >/dev/null 2>&1 || { echo "ERROR: Rust toolchain install failed (cargo not found)" >&2; exit 1; }

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
if ! printf '%s' "$BASELINE_COMMIT" | grep -qE '^[0-9a-fA-F]{40}$'; then
    echo "ERROR: ${BASELINE_FILE} does not contain a valid 40-character commit SHA (got: '${BASELINE_COMMIT}')" >&2
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

# Compile one side's bench binaries with human-readable output so any compile
# error is visible in the log and fails the run loudly. bench_bins() below hides
# cargo's stderr and only extracts paths, so a compile failure there would abort
# with no diagnostics.
compile_benches() {
    echo ">>> Compiling $2 bench binaries ($1)..."
    if ! CARGO_TARGET_DIR="$1" cargo bench -p mssql-tds-bench --no-run; then
        echo "ERROR: $2 bench compilation failed — see the cargo errors above." >&2
        exit 1
    fi
}

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
compile_benches "$REPO_ROOT/target" "candidate"
CAND_BINS="$(bench_bins "$REPO_ROOT/target")"
[ -n "$CAND_BINS" ] || { echo "ERROR: no candidate bench binaries found"; exit 1; }

BASELINE_TREE="$(mktemp -d)/perf-baseline"
echo ">>> Adding baseline worktree for ${BASELINE_COMMIT} at ${BASELINE_TREE}..."
git worktree add --detach "$BASELINE_TREE" "$BASELINE_COMMIT"
echo ">>> Building baseline bench binaries (target-base/)..."
swap_to_baseline
# From here until the swap is undone, any exit (a baseline compile failure, or
# set -e on anything else) would otherwise leave mssql-tds/ holding the baseline
# source and the candidate stranded in .mssql-tds-candidate.
trap 'restore_candidate 2>/dev/null || true; git worktree remove --force "$BASELINE_TREE" 2>/dev/null || true' EXIT
compile_benches "$REPO_ROOT/target-base" "baseline"
BASE_BINS="$(bench_bins "$REPO_ROOT/target-base")"
restore_candidate
git worktree remove --force "$BASELINE_TREE" || true
trap - EXIT
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

THR="${BENCH_REGRESSION_RATIO:-1.10}"
# Improvements are verified at the SAME magnitude as regressions by default: a
# baseline-slower anomaly pollutes the recorded numbers (and the run-over-run
# trend they feed) exactly as much as a candidate-slower one, and both directions
# share one re-measure set, so the extra confidence costs nothing per run.
IMP_THR="${BENCH_IMPROVEMENT_VERIFY_RATIO:-$THR}"

# Print the IDs (field 1) of benchmarks whose candidate ratio (field 6) meets or
# exceeds the regression threshold, one per line.
regression_ids() {
    awk -v thr="$THR" '
        $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ && ($6 + 0) >= thr { print $1 }
    ' "$1"
}

# Like regression_ids, but prints "id candidate_ratio" so the auto-confirm loop
# can tally how many re-runs each benchmark tripped and track its worst ratio.
regression_pairs() {
    awk -v thr="$THR" '
        $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ && ($6 + 0) >= thr { print $1, $6 }
    ' "$1"
}

OFFENDERS=$(regression_ids "$RESULTS_DIR/comparison.txt")

# The gate is one-directional, so a *baseline*-slower result is never challenged
# and an unverified "3x faster" gets published. critcmp normalizes the faster
# side to 1.00, so field 2 carries the baseline's ratio: select IDs where the
# baseline is slower by at least IMP_THR and re-measure them too.
improvement_ids() {
    awk -v thr="$IMP_THR" '
        $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ && ($2 + 0) >= thr { print $1 }
    ' "$1"
}
improvement_pairs() {
    awk -v thr="$IMP_THR" '
        $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ && ($2 + 0) >= thr { print $1, $2 }
    ' "$1"
}

IMPROVEMENTS=$(improvement_ids "$RESULTS_DIR/comparison.txt")
# One re-measure set covers both directions, so the re-runs cost one pass.
# awk 'NF' drops the blank lines rather than grep, which would exit 1 when both
# lists are empty (a clean run) and kill the script under set -e.
VERIFY_IDS=$(printf '%s\n%s\n' "$OFFENDERS" "$IMPROVEMENTS" | awk 'NF' | sort -u)

# --- Auto-confirm regressions: re-measure the offenders N times, require a
# --- majority to confirm ---
# A strict gate can trip on a transient single-benchmark outlier — short,
# CPU-bound benches (e.g. the decode microbenches) can swing double digits on a
# shared VM. So re-measure ONLY the benchmarks that tripped — interleaved per
# binary, same as the main run — several times, and keep as a real regression
# only those that trip in a MAJORITY of the re-runs. A true regression reproduces
# consistently; noise does not. Both bench binaries are already built and the
# offenders are a small subset, so the extra re-runs stay cheap.
#   BENCH_CONFIRM_RUNS   (default 4)              — number of re-runs
#   BENCH_CONFIRM_QUORUM (default majority = N/2+1) — re-runs required to confirm
CONFIRM_RUNS="${BENCH_CONFIRM_RUNS:-4}"
# Reject settings that would silently disable the gate rather than tune it:
# CONFIRM_RUNS=0 skips the loop and clears every regression, and a quorum above
# the run count can never be met, so nothing is ever confirmed. CONFIRM_RUNS is
# checked before QUORUM is derived, since that default is an arithmetic
# expansion that would fail confusingly on a non-numeric value.
case "$CONFIRM_RUNS" in ''|*[!0-9]*) echo "ERROR: BENCH_CONFIRM_RUNS must be a positive integer (got: '${CONFIRM_RUNS}')" >&2; exit 1 ;; esac
if [ "$CONFIRM_RUNS" -lt 1 ]; then
    echo "ERROR: BENCH_CONFIRM_RUNS must be >= 1 (got: ${CONFIRM_RUNS}); 0 would clear every regression unconfirmed." >&2
    exit 1
fi
QUORUM="${BENCH_CONFIRM_QUORUM:-$(( CONFIRM_RUNS / 2 + 1 ))}"
case "$QUORUM" in ''|*[!0-9]*) echo "ERROR: BENCH_CONFIRM_QUORUM must be a positive integer (got: '${QUORUM}')" >&2; exit 1 ;; esac
if [ "$QUORUM" -lt 1 ] || [ "$QUORUM" -gt "$CONFIRM_RUNS" ]; then
    echo "ERROR: BENCH_CONFIRM_QUORUM must be between 1 and BENCH_CONFIRM_RUNS (got: ${QUORUM} of ${CONFIRM_RUNS})." >&2
    exit 1
fi
CONFIRMED_IDS=""
IMP_CONFIRMED=""
TALLY_FILE="$RESULTS_DIR/confirm-tally.txt"
IMP_TALLY_FILE="$RESULTS_DIR/improvement-tally.txt"
: > "$TALLY_FILE"
: > "$IMP_TALLY_FILE"

if [ -n "$VERIFY_IDS" ]; then
    FILTER=$(printf '%s\n' "$VERIFY_IDS" | sed 's|^|^|; s|$|$|' | paste -sd '|' -)
    [ -n "$OFFENDERS" ] && echo ">>> Gate tripped by: $(printf '%s ' $OFFENDERS)"
    [ -n "$IMPROVEMENTS" ] && echo ">>> Verifying large apparent improvement(s): $(printf '%s ' $IMPROVEMENTS)"
    echo ">>> Auto-confirm: re-measuring those benchmark(s) ${CONFIRM_RUNS}x; a result counts only if it reproduces in >= ${QUORUM} of ${CONFIRM_RUNS} re-runs."
    # One warm-up before the loop; the re-runs are back-to-back so caches stay hot.
    warmup_pass "$FILTER"
    for run in $(seq 1 "$CONFIRM_RUNS"); do
        echo ">>> Auto-confirm re-run ${run}/${CONFIRM_RUNS}..."
        interleave_run "candidate_confirm${run}" "base_confirm${run}" "$FILTER"
        critcmp "base_confirm${run}" "candidate_confirm${run}" | tee "$RESULTS_DIR/confirm-run${run}.txt"
        regression_pairs "$RESULTS_DIR/confirm-run${run}.txt" >> "$TALLY_FILE"
        improvement_pairs "$RESULTS_DIR/confirm-run${run}.txt" >> "$IMP_TALLY_FILE"
    done
    # Confirmed = benchmarks that tripped in at least QUORUM of the re-runs.
    CONFIRMED_IDS=$(awk '{ print $1 }' "$TALLY_FILE" | sort | uniq -c \
        | awk -v q="$QUORUM" '$1 >= q { print $2 }')
    IMP_CONFIRMED=$(awk '{ print $1 }' "$IMP_TALLY_FILE" | sort | uniq -c \
        | awk -v q="$QUORUM" '$1 >= q { print $2 }')
fi
rm -rf "$REPO_ROOT/target-base" 2>/dev/null || true

# Per-offender trip count across the re-runs (0 if it never re-tripped).
offender_hits() { awk -v id="$1" '$1 == id { c++ } END { print c + 0 }' "$TALLY_FILE"; }
# Per-offender worst candidate ratio among the re-runs it tripped ("" if none).
offender_worst() { awk -v id="$1" '$1 == id && $2 + 0 > w { w = $2 + 0 } END { if (w > 0) print w }' "$TALLY_FILE"; }
# Same, for apparent improvements (how many re-runs reproduced it, and the best
# baseline-slower ratio seen).
imp_hits() { awk -v id="$1" '$1 == id { c++ } END { print c + 0 }' "$IMP_TALLY_FILE"; }
imp_best() { awk -v id="$1" '$1 == id && $2 + 0 > w { w = $2 + 0 } END { if (w > 0) print w }' "$IMP_TALLY_FILE"; }

# Candidate/base ratio for a benchmark id in a critcmp file ("" if absent).
ratio_in_file() { awk -v id="$1" '$1 == id && $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ { print $6 / $2; exit }' "$2"; }
# Median of the numbers read on stdin.
median_stdin() { sort -n | awk '{ v[NR] = $1 } END { if (NR == 0) exit; if (NR % 2) print v[(NR + 1) / 2]; else print (v[NR / 2] + v[NR / 2 + 1]) / 2 }'; }

# Reconcile each re-measured benchmark's headline number with the gate: replace
# its first-pass ratio with the MEDIAN OF THE RE-RUNS, the same measurements the
# quorum counts. The first pass is excluded deliberately: a benchmark is only
# re-measured because that pass was extreme, so including it re-counts the very
# outlier under test and would give it a tie-breaking vote the gate does not
# have (2-of-4 re-runs clears the gate, yet 3 of those 5 values are trips, so the
# median could stay above the threshold and contradict a passing verdict).
# The raw critcmp block in the summary keeps the untouched first-pass data.
MEDIANS_FILE="$RESULTS_DIR/offender-medians.txt"
: > "$MEDIANS_FILE"
for id in $VERIFY_IDS; do
    med=$(
        {
            for run in $(seq 1 "$CONFIRM_RUNS"); do ratio_in_file "$id" "$RESULTS_DIR/confirm-run${run}.txt"; done
        } | awk '/^[0-9.]+$/' | median_stdin
    ) || med=""
    if [ -n "$med" ]; then printf '%s %s\n' "$id" "$med" >> "$MEDIANS_FILE"; fi
done

# --- Verdict (based on the majority-confirmed regressions) ---
PCT=$(awk -v t="$THR" 'BEGIN { printf "%d", (t - 1) * 100 + 0.5 }')
IMP_PCT=$(awk -v t="$IMP_THR" 'BEGIN { printf "%d", (t - 1) * 100 + 0.5 }')
NCONF=$(printf '%s\n' ${CONFIRMED_IDS:-} | grep -c . || true)
if [ "${NCONF:-0}" -gt 0 ]; then
    # Worst confirmed benchmark by its max observed ratio across the re-runs.
    WLINE=$(for id in $CONFIRMED_IDS; do echo "$(offender_worst "$id") $id $(offender_hits "$id")"; done | sort -rn | head -1)
    WNAME=$(echo "$WLINE" | awk '{ print $2 }')
    WPCT=$(echo "$WLINE" | awk '{ printf "%d", ($1 - 1) * 100 + 0.5 }')
    WHITS=$(echo "$WLINE" | awk '{ print $3 }')
    VERDICT=$(printf "\342\232\240\357\270\217 %d benchmark(s) consistently slower by >=%d%% vs baseline (worst: %s +%d%%, tripped %s/%s re-runs)" "$NCONF" "$PCT" "$WNAME" "$WPCT" "$WHITS" "$CONFIRM_RUNS")
else
    VERDICT=$(printf "\342\234\205 No benchmark consistently slower by >=%d%% vs baseline" "$PCT")
fi

# Emit each benchmark's % change as a compact, colored "diverging bar" in a
# GitHub/ADO-flavored markdown table (renders with color on the run Summary tab,
# unlike the fixed-width critcmp block). Green = faster, red = slower, one square
# per ~1%, drawn only outside ±1% so the noise rows stay clean. Reads the critcmp
# table ($2 = base ratio, $6 = candidate ratio; % change = candidate/base - 1).
# $2 = optional "id ratio" overrides file: re-measured offenders use that median
# ratio (marked ⟳) instead of their first-pass value.
emoji_bar_table() {
    awk -v g="🟩" -v r="🟥" -v ov="${2:-}" '
        BEGIN {
            if (ov != "") while ((getline line < ov) > 0) { split(line, kv, " "); over[kv[1]] = kv[2]; }
        }
        $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ {
            m++; id[m] = $1;
            if (id[m] in over) { pct[m] = (over[id[m]] - 1) * 100; rem[m] = 1; }
            else               { pct[m] = ($6 / $2 - 1) * 100; }
        }
        END {
            # sort indices ascending by % change (fastest first)
            for (i = 1; i <= m; i++)
                for (j = i + 1; j <= m; j++)
                    if (pct[j] < pct[i]) {
                        t = pct[i]; pct[i] = pct[j]; pct[j] = t;
                        s = id[i];  id[i] = id[j];  id[j] = s;
                        u = rem[i]; rem[i] = rem[j]; rem[j] = u;
                    }
            print "| Benchmark | faster \342\227\204 | \316\224% | \342\226\272 slower |";
            print "|---|--:|:--:|:--|";
            for (i = 1; i <= m; i++) {
                p = pct[i]; a = (p < 0) ? -p : p;
                n = int(a + 0.5); if (n > 12) n = 12;
                gs = ""; rs = "";
                if (p <= -1)     { for (q = 0; q < n; q++) gs = gs g; }
                else if (p >= 1) { for (q = 0; q < n; q++) rs = rs r; }
                if (p <= -0.05)      lbl = sprintf("%.1f", p);
                else if (p >= 0.05)  lbl = sprintf("+%.1f", p);
                else                 lbl = "\302\2610.0";
                mark = rem[i] ? " \342\237\263" : "";
                printf "| `%s`%s | %s | %s | %s |\n", id[i], mark, gs, lbl, rs;
            }
        }
    ' "$1"
}

# Markdown summary — the perf lab attaches results/*.md to the run's Summary tab
# (task.uploadsummary), so the comparison renders inline on the run page. The
# critcmp table is fixed-width, so wrap it in a fenced code block to keep it
# aligned.
{
    echo "## mssql-tds perf — base → candidate"
    echo ""
    echo "**${VERDICT}**"
    echo ""
    if [ -n "$OFFENDERS" ]; then
        echo "_Auto-confirm re-measured the initially-tripping benchmark(s) ${CONFIRM_RUNS}× (interleaved, offenders only). A regression is counted only when it trips in at least ${QUORUM} of ${CONFIRM_RUNS} re-runs; a benchmark that spikes once but not consistently is treated as transient noise._"
        echo ""
    fi
    if [ -n "$IMPROVEMENTS" ]; then
        echo "_Benchmark(s) where the baseline looked slower by ≥${IMP_PCT}% were re-measured the same way, so an apparent win that does not reproduce is not reported as real._"
        echo ""
    fi
    echo "### Change vs baseline"
    echo ""
    echo "_🟩 faster · 🟥 slower · 1 square ≈ 1% (drawn only for |Δ| ≥ 1%) · ⟳ re-measured (median of re-runs)_"
    echo ""
    emoji_bar_table "$RESULTS_DIR/comparison.txt" "$MEDIANS_FILE"
    echo ""
    echo "Baseline commit: \`${BASELINE_COMMIT}\`"
    echo ""
    echo "### Raw first-pass measurements"
    echo ""
    echo "_Full critcmp table from the initial run. Benchmarks marked ⟳ above were re-measured; the chart shows the median and the re-runs are detailed below._"
    echo ""
    echo '```'
    cat "$RESULTS_DIR/comparison.txt"
    echo '```'
    if [ -n "$OFFENDERS" ]; then
        echo ""
        echo "### Regressions (auto-confirm)"
        echo ""
        echo "Initially tripped: $(printf '%s ' $OFFENDERS)"
        echo ""
        echo "| benchmark | re-runs tripped | worst |"
        echo "|-----------|-----------------|-------|"
        for id in $OFFENDERS; do
            hits=$(offender_hits "$id")
            w=$(offender_worst "$id")
            if [ -n "$w" ]; then
                wcell=$(awk -v r="$w" 'BEGIN { printf "+%d%%", (r - 1) * 100 + 0.5 }')
            else
                wcell="—"
            fi
            echo "| ${id} | ${hits}/${CONFIRM_RUNS} | ${wcell} |"
        done
        echo ""
        echo "_Confirmed (tripped in ≥ ${QUORUM}/${CONFIRM_RUNS}): ${CONFIRMED_IDS:-none}_"
        echo ""
    fi
    if [ -n "$IMPROVEMENTS" ]; then
        echo ""
        echo "### Large improvements (verification)"
        echo ""
        echo "_Baseline slower by ≥${IMP_PCT}%. These never fail the gate; they are re-measured so a one-off artifact is not published as a real gain. A win that **does** reproduce is also worth a look — it can mean the candidate is doing less work rather than the same work faster._"
        echo ""
        echo "| benchmark | reproduced | best |"
        echo "|-----------|------------|------|"
        for id in $IMPROVEMENTS; do
            ihits=$(imp_hits "$id")
            ib=$(imp_best "$id")
            if [ -n "$ib" ]; then
                ibcell=$(awk -v r="$ib" 'BEGIN { printf "%.2fx faster", r }')
            else
                ibcell="—"
            fi
            echo "| ${id} | ${ihits}/${CONFIRM_RUNS} | ${ibcell} |"
        done
        echo ""
        echo "_Verified (reproduced in ≥ ${QUORUM}/${CONFIRM_RUNS}): ${IMP_CONFIRMED:-none}_"
        echo ""
    fi
    if [ -n "$VERIFY_IDS" ]; then
        echo ""
        echo "### Re-run detail (re-measured benchmarks only)"
        echo ""
        for run in $(seq 1 "$CONFIRM_RUNS"); do
            echo "#### Re-run ${run}"
            echo ""
            echo '```'
            cat "$RESULTS_DIR/confirm-run${run}.txt"
            echo '```'
            echo ""
        done
    fi
} > "$RESULTS_DIR/summary.md"

# Also echo the summary into the log: task.uploadsummary only surfaces it on the
# run's Summary tab, so without this the verdict is invisible when triaging from
# the log alone.
echo ""
echo "===== summary.md ====="
cat "$RESULTS_DIR/summary.md"
echo "===== end summary.md ====="
echo ""

# Archive the raw Criterion data for offline analysis.
cp -r target/criterion "$RESULTS_DIR/criterion" 2>/dev/null || true

echo ">>> Done. Results in ${RESULTS_DIR}"

# Fail the run only on CONFIRMED regressions (tripped in a majority of re-runs).
if [ "${NCONF:-0}" -gt 0 ]; then
    echo ">>> ${VERDICT}"
    echo ">>> FAILING: ${NCONF} benchmark(s) regressed in >= ${QUORUM} of ${CONFIRM_RUNS} auto-confirm re-runs (BENCH_REGRESSION_RATIO=${THR})."
    exit 1
fi
if [ -n "$OFFENDERS" ]; then
    echo ">>> Auto-confirm cleared all $(printf '%s\n' $OFFENDERS | grep -c .) initial regression(s) as transient (none tripped in >= ${QUORUM}/${CONFIRM_RUNS}); passing."
fi
for id in ${IMPROVEMENTS:-}; do
    ihits=$(imp_hits "$id")
    if [ "$ihits" -lt "$QUORUM" ]; then
        echo ">>> NOTE: apparent improvement in '${id}' did not reproduce (${ihits}/${CONFIRM_RUNS}); reported as a measurement artifact, not a real gain."
    fi
done

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

# --- Connection (SQL_SERVER / SQL_PASSWORD injected by run-remote.sh) ---
export DB_HOST="${SQL_SERVER:?SQL_SERVER not set}"
export DB_PORT="${DB_PORT:-1433}"
export DB_USERNAME="${DB_USERNAME:-sa}"
export TRUST_SERVER_CERTIFICATE="${TRUST_SERVER_CERTIFICATE:-true}"
# SQL_PASSWORD is already exported into this session by run-remote.sh.
: "${SQL_PASSWORD:?SQL_PASSWORD not set}"

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

# --- Warm-up pass (discarded) ---
# Candidate is measured first and baseline second, so without this the candidate
# pays a cold-cache penalty (SQL buffer pool, memory grants, OS pages) that the
# baseline run doesn't — which shows up as a spurious delta on the largest
# working-set benches (e.g. the 20 MB LOB). Run the whole suite once, fast and
# discarded, to prime SQL Server and the OS so BOTH measured runs start warm.
echo ">>> Warm-up pass (discarded)..."
BENCH_WARMUP_SECS=1 BENCH_SECS=1 BENCH_SAMPLES=10 \
    "${BENCH_PREFIX[@]}" cargo bench -p mssql-tds-bench -- --save-baseline warmup >/dev/null 2>&1 || true

# --- Candidate run (mssql-tds = working tree) ---
echo ">>> Candidate benchmarks..."
"${BENCH_PREFIX[@]}" cargo bench -p mssql-tds-bench -- --save-baseline candidate

# --- Baseline run (mssql-tds source swapped to the baseline commit) ---
# Materialize the baseline mssql-tds via a local worktree, then replace the
# workspace's mssql-tds source in place. The harness (mssql-tds-bench) and its
# `path = "../mssql-tds"` dependency are unchanged, so mssql-tds is the only
# variable. Swapping the source (rather than re-pointing the dependency at the
# worktree) keeps a single mssql-tds in the workspace and avoids a Cargo lockfile
# package collision.
BASELINE_TREE="$(mktemp -d)/perf-baseline"
echo ">>> Adding baseline worktree for ${BASELINE_COMMIT} at ${BASELINE_TREE}..."
git worktree add --detach "$BASELINE_TREE" "$BASELINE_COMMIT"

echo ">>> Swapping mssql-tds source to the baseline..."
mv "$REPO_ROOT/mssql-tds" "$REPO_ROOT/.mssql-tds-candidate"
cp -r "$BASELINE_TREE/mssql-tds" "$REPO_ROOT/mssql-tds"

echo ">>> Baseline benchmarks..."
"${BENCH_PREFIX[@]}" cargo bench -p mssql-tds-bench -- --save-baseline base

# Restore the candidate source and remove the worktree.
rm -rf "$REPO_ROOT/mssql-tds"
mv "$REPO_ROOT/.mssql-tds-candidate" "$REPO_ROOT/mssql-tds"
git worktree remove --force "$BASELINE_TREE" || true

# --- Compare (both baselines live in the shared target/criterion) ---
echo ">>> Comparing base -> candidate..."
critcmp base candidate | tee "$RESULTS_DIR/comparison.txt"

# Markdown summary — the perf lab attaches results/*.md to the run's Summary tab
# (task.uploadsummary), so the comparison renders inline on the run page. The
# critcmp table is fixed-width, so wrap it in a fenced code block to keep it
# aligned.
#
# Verdict: in each critcmp data row the faster side is 1.00 and the slower side
# shows its ratio, so the candidate regressed a bench when the candidate ratio
# (field 6) exceeds the threshold. Flag any candidate regression >= the ratio.
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
' "$RESULTS_DIR/comparison.txt")

{
    echo "## mssql-tds perf — base → candidate"
    echo ""
    echo "**${VERDICT}**"
    echo ""
    echo "Baseline commit: \`${BASELINE_COMMIT}\`"
    echo ""
    echo '```'
    cat "$RESULTS_DIR/comparison.txt"
    echo '```'
} > "$RESULTS_DIR/summary.md"

# Archive the raw Criterion data for offline analysis.
cp -r target/criterion "$RESULTS_DIR/criterion" 2>/dev/null || true

echo ">>> Done. Results in ${RESULTS_DIR}"

# Fail the run when any benchmark regressed past the threshold so the pipeline
# surfaces it. Uses the same rule as the verdict above: candidate ratio (field 6)
# >= BENCH_REGRESSION_RATIO. summary.md (attached to the run) and the critcmp
# table above name the offenders; the standard triage is to re-run to confirm a
# real regression versus run-to-run noise.
REGRESSIONS=$(awk -v thr="${BENCH_REGRESSION_RATIO:-1.10}" '
    $2 ~ /^[0-9]+\.[0-9]+$/ && $6 ~ /^[0-9]+\.[0-9]+$/ && ($6 + 0) >= thr { n++ }
    END { print n + 0 }
' "$RESULTS_DIR/comparison.txt")
if [ "${REGRESSIONS:-0}" -gt 0 ]; then
    echo ">>> ${VERDICT}"
    echo ">>> FAILING: ${REGRESSIONS} benchmark(s) exceeded the regression threshold (BENCH_REGRESSION_RATIO=${BENCH_REGRESSION_RATIO:-1.10})."
    exit 1
fi

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

# --- Candidate run (mssql-tds = working tree) ---
echo ">>> Candidate benchmarks..."
cargo bench -p mssql-tds-bench -- --save-baseline candidate

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
cargo bench -p mssql-tds-bench -- --save-baseline base

# Restore the candidate source and remove the worktree.
rm -rf "$REPO_ROOT/mssql-tds"
mv "$REPO_ROOT/.mssql-tds-candidate" "$REPO_ROOT/mssql-tds"
git worktree remove --force "$BASELINE_TREE" || true

# --- Compare (both baselines live in the shared target/criterion) ---
echo ">>> Comparing base -> candidate..."
critcmp base candidate | tee "$RESULTS_DIR/comparison.txt"

# Archive the raw Criterion data for offline analysis.
cp -r target/criterion "$RESULTS_DIR/criterion" 2>/dev/null || true

echo ">>> Done. Results in ${RESULTS_DIR}"

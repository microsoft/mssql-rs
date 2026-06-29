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
# swapping ONLY the mssql-tds dependency:
#   * candidate: mssql-tds = path ../mssql-tds (the working tree)
#   * baseline:  mssql-tds = path to a local `git worktree` of the
#                `perf-baseline` tag (no ADO auth needed on the VM)
# Any statistically significant delta is therefore attributable to mssql-tds.
set -euo pipefail

# Baseline pointer — hard-coded. The `perf-baseline` tag is moved manually in
# the git repo when the baseline should advance.
BASELINE_TAG="perf-baseline"

REPO_ROOT="$(pwd)"
RESULTS_DIR="$REPO_ROOT/results"
MANIFEST="$REPO_ROOT/mssql-tds-bench/Cargo.toml"
mkdir -p "$RESULTS_DIR"

# --- Connection (SQL_SERVER / SQL_PASSWORD injected by run-remote.sh) ---
export DB_HOST="${SQL_SERVER:?SQL_SERVER not set}"
export DB_PORT="${DB_PORT:-1433}"
export DB_USERNAME="${DB_USERNAME:-sa}"
export TRUST_SERVER_CERTIFICATE="${TRUST_SERVER_CERTIFICATE:-true}"
# SQL_PASSWORD is already exported into this session by run-remote.sh.
: "${SQL_PASSWORD:?SQL_PASSWORD not set}"

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

# --- Verify the baseline tag is present in the shipped .git ---
if ! git rev-parse "refs/tags/${BASELINE_TAG}^{commit}" >/dev/null 2>&1; then
    echo "ERROR: baseline tag '${BASELINE_TAG}' not found in the shipped repository." >&2
    echo "       Ensure the pipeline checkout fetches tags and the tag exists." >&2
    exit 1
fi

# --- Candidate run (mssql-tds = working tree) ---
echo ">>> Candidate benchmarks..."
cargo bench -p mssql-tds-bench -- --save-baseline candidate

# --- Baseline run (mssql-tds = perf-baseline tag via a local worktree) ---
BASELINE_TREE="$(mktemp -d)/perf-baseline"
echo ">>> Adding baseline worktree for tag '${BASELINE_TAG}' at ${BASELINE_TREE}..."
git worktree add --detach "$BASELINE_TREE" "refs/tags/${BASELINE_TAG}"

echo ">>> Swapping mssql-tds dependency to the baseline source..."
cp "$MANIFEST" "$MANIFEST.bak"
sed -i "s|mssql-tds = { path = \"../mssql-tds\" }|mssql-tds = { path = \"${BASELINE_TREE}/mssql-tds\" }|" "$MANIFEST"
grep -q "path = \"${BASELINE_TREE}/mssql-tds\"" "$MANIFEST" \
    || { echo "ERROR: failed to swap mssql-tds dependency" >&2; exit 1; }

echo ">>> Baseline benchmarks..."
cargo bench -p mssql-tds-bench -- --save-baseline base

# Restore the committed manifest and remove the worktree.
mv "$MANIFEST.bak" "$MANIFEST"
git worktree remove --force "$BASELINE_TREE" || true

# --- Compare (both baselines live in the shared target/criterion) ---
echo ">>> Comparing base -> candidate..."
critcmp base candidate | tee "$RESULTS_DIR/comparison.txt"

# Archive the raw Criterion data for offline analysis.
cp -r target/criterion "$RESULTS_DIR/criterion" 2>/dev/null || true

echo ">>> Done. Results in ${RESULTS_DIR}"

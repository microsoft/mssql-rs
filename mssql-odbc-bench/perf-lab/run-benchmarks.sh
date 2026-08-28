#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Dedicated Linux perf-lab runner for the mssql-odbc result-set benchmarks.

set -euo pipefail
set -E
trap 'rc=$?; echo "ERROR: ${BASH_SOURCE[0]}:${LINENO}: ${BASH_COMMAND} exited ${rc}" >&2' ERR

REPO_ROOT="$(pwd)"
RESULTS_DIR="$REPO_ROOT/results"
BASELINE_FILE="$REPO_ROOT/mssql-odbc-bench/perf-lab/baseline-commit.txt"
REFERENCE_VERSION_FILE="$REPO_ROOT/mssql-odbc-bench/perf-lab/msodbcsql-version.txt"
HARNESS_BUILD_DIR="$REPO_ROOT/target/odbc-bench"
CANDIDATE_TARGET_DIR="$REPO_ROOT/target/odbc-candidate"
BASELINE_TARGET_DIR="$REPO_ROOT/target/odbc-baseline"
CANDIDATE_DRIVER_NAME="MSSQL Rust ODBC Perf Candidate"
BASELINE_DRIVER_NAME="MSSQL Rust ODBC Perf Baseline"
MICROSOFT_DRIVER_NAME="ODBC Driver 18 for SQL Server"

BASELINE_TEMP_DIR=""
BASELINE_TREE=""
DRIVER_INI_DIR=""
ADMIN_EXE=""
TABLE_CLEANUP_ARMED=0

cleanup() {
    # Keep failed runs from leaving tables, worktrees, or temporary driver catalogs
    # that could change the next run's result.
    local rc=$?
    trap - ERR
    set +e

    if [ "$TABLE_CLEANUP_ARMED" -eq 1 ] && [ -x "$ADMIN_EXE" ] &&
       [ -n "$DRIVER_INI_DIR" ]; then
        echo ">>> Removing ODBC benchmark tables..."
        ODBCSYSINI="$DRIVER_INI_DIR" \
            ODBC_BENCH_DRIVER="$CANDIDATE_DRIVER_NAME" \
            ODBC_BENCH_SCENARIO="" \
            "$ADMIN_EXE" cleanup ||
            echo "WARNING: benchmark table cleanup failed" >&2
    fi

    if [ -n "$BASELINE_TREE" ]; then
        git worktree remove --force "$BASELINE_TREE" >/dev/null 2>&1 || true
    fi
    if [ -n "$BASELINE_TEMP_DIR" ] && [ -d "$BASELINE_TEMP_DIR" ]; then
        rm -rf "$BASELINE_TEMP_DIR"
    fi
    if [ -n "$DRIVER_INI_DIR" ] && [ -d "$DRIVER_INI_DIR" ]; then
        rm -rf "$DRIVER_INI_DIR"
    fi
    exit "$rc"
}
trap cleanup EXIT

mkdir -p "$RESULTS_DIR"

: "${SQL_SERVER:?SQL_SERVER not set}"
: "${SQL_PASSWORD:?SQL_PASSWORD not set}"

export ODBC_BENCH_SERVER="${ODBC_BENCH_SERVER:-$SQL_SERVER}"
export ODBC_BENCH_DATABASE="${ODBC_BENCH_DATABASE:-tempdb}"
export ODBC_BENCH_UID="${ODBC_BENCH_UID:-${DB_USERNAME:-sa}}"
export ODBC_BENCH_PWD="${ODBC_BENCH_PWD:-$SQL_PASSWORD}"
export ODBC_BENCH_TRUST_CERT="${ODBC_BENCH_TRUST_CERT:-Yes}"
export ODBC_BENCH_ENCRYPT="${ODBC_BENCH_ENCRYPT:-Mandatory}"
export ODBC_BENCH_PACKET_SIZE="${ODBC_BENCH_PACKET_SIZE:-32768}"

REPETITIONS="${ODBC_BENCH_REPETITIONS:-5}"
case "$REPETITIONS" in
    ''|*[!0-9]*)
        echo "ERROR: ODBC_BENCH_REPETITIONS must be a positive integer" >&2
        exit 1
        ;;
esac
REPETITIONS=$((10#$REPETITIONS))
if [ "$REPETITIONS" -lt 1 ]; then
    echo "ERROR: ODBC_BENCH_REPETITIONS must be at least 1" >&2
    exit 1
fi

ensure_packages() {
    # Perf images vary by pool, so install only tools the harness cannot run without.
    local missing=()
    command -v git >/dev/null 2>&1 || missing+=(git)
    command -v curl >/dev/null 2>&1 || missing+=(curl)
    command -v python3 >/dev/null 2>&1 || missing+=(python3)
    command -v cmake >/dev/null 2>&1 || missing+=(cmake)
    command -v c++ >/dev/null 2>&1 || missing+=(build-essential)
    command -v pkg-config >/dev/null 2>&1 || missing+=(pkg-config)
    [ -f /usr/include/sql.h ] || missing+=(unixodbc-dev)
    [ -f /usr/include/openssl/ssl.h ] || missing+=(libssl-dev)
    [ -f /etc/ssl/certs/ca-certificates.crt ] || missing+=(ca-certificates)
    [ ${#missing[@]} -eq 0 ] && return

    local sudo_command=()
    [ "$(id -u)" -ne 0 ] && sudo_command=(sudo)
    echo ">>> Installing system packages: ${missing[*]}"
    "${sudo_command[@]}" apt-get update -y
    "${sudo_command[@]}" env DEBIAN_FRONTEND=noninteractive \
        apt-get install -y --no-install-recommends "${missing[@]}"
}
ensure_packages

if ! command -v cargo >/dev/null 2>&1; then
    echo ">>> Installing Rust toolchain..."
    bash "$REPO_ROOT/.pipeline/scripts/install-rustup.sh"
fi
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null 2>&1 ||
    { echo "ERROR: cargo not found after Rust setup" >&2; exit 1; }

if [ ! -f "$BASELINE_FILE" ]; then
    echo "ERROR: baseline file not found: $BASELINE_FILE" >&2
    exit 1
fi
BASELINE_COMMIT="$(
    grep -vE '^[[:space:]]*(#|$)' "$BASELINE_FILE" |
        head -n1 |
        tr -d '[:space:]'
)"
if ! printf '%s' "$BASELINE_COMMIT" | grep -qE '^[0-9a-fA-F]{40}$'; then
    echo "ERROR: invalid baseline commit in $BASELINE_FILE" >&2
    exit 1
fi
if ! git rev-parse --verify --quiet "${BASELINE_COMMIT}^{commit}" >/dev/null; then
    echo "ERROR: baseline commit $BASELINE_COMMIT is absent from the checkout" >&2
    exit 1
fi

if [ ! -f "$REFERENCE_VERSION_FILE" ]; then
    echo "ERROR: reference version file not found: $REFERENCE_VERSION_FILE" >&2
    exit 1
fi
MICROSOFT_VERSION="$(
    grep -vE '^[[:space:]]*(#|$)' "$REFERENCE_VERSION_FILE" |
        head -n1 |
        tr -d '[:space:]'
)"
if ! printf '%s' "$MICROSOFT_VERSION" |
    grep -qE '^[0-9]+(\.[0-9]+){3}$'; then
    echo "ERROR: invalid Microsoft ODBC version in $REFERENCE_VERSION_FILE" >&2
    exit 1
fi
"$REPO_ROOT/.pipeline/scripts/install-msodbcsql.sh" "$MICROSOFT_VERSION"
MICROSOFT_PACKAGE_VERSION="$(dpkg-query -W -f='${Version}' msodbcsql18)"
mapfile -t microsoft_driver_paths < <(
    dpkg-query -L msodbcsql18 |
        grep -E '/libmsodbcsql-[0-9.]+\.so\.[0-9.]+$' || true
)
if [ "${#microsoft_driver_paths[@]}" -ne 1 ]; then
    echo "ERROR: expected one installed Microsoft ODBC shared library" >&2
    printf '  %s\n' "${microsoft_driver_paths[@]}" >&2
    exit 1
fi
MICROSOFT_DRIVER="${microsoft_driver_paths[0]}"
MICROSOFT_DRIVER_SHA256="$(sha256sum "$MICROSOFT_DRIVER" | cut -d' ' -f1)"

echo ">>> Building the fixed C++ benchmark harness..."
cmake -S "$REPO_ROOT/mssql-odbc-bench" \
    -B "$HARNESS_BUILD_DIR" \
    -DCMAKE_BUILD_TYPE=Release
cmake --build "$HARNESS_BUILD_DIR" --config Release --parallel

build_driver() {
    # Separate target directories keep the two Rust driver builds comparable and
    # prevent Cargo from reusing candidate artifacts for the pinned baseline.
    local source_root="$1"
    local target_dir="$2"
    local label="$3"
    echo ">>> Building $label mssql-odbc driver..."
    (
        cd "$source_root"
        CARGO_TARGET_DIR="$target_dir" cargo build -p mssql-odbc --release
    )
}

build_driver "$REPO_ROOT" "$CANDIDATE_TARGET_DIR" "candidate"

BASELINE_TEMP_DIR="$(mktemp -d)"
BASELINE_TREE="$BASELINE_TEMP_DIR/worktree"
echo ">>> Adding baseline worktree for $BASELINE_COMMIT..."
git worktree add --detach "$BASELINE_TREE" "$BASELINE_COMMIT"
build_driver "$BASELINE_TREE" "$BASELINE_TARGET_DIR" "baseline"

CANDIDATE_DRIVER="$CANDIDATE_TARGET_DIR/release/libmsodbcsql18.so"
BASELINE_DRIVER="$BASELINE_TARGET_DIR/release/libmsodbcsql18.so"
BENCH_EXE="$HARNESS_BUILD_DIR/mssql_odbc_bench"
ADMIN_EXE="$HARNESS_BUILD_DIR/mssql_odbc_bench_admin"
for required_file in \
    "$CANDIDATE_DRIVER" "$BASELINE_DRIVER" "$MICROSOFT_DRIVER" \
    "$BENCH_EXE" "$ADMIN_EXE"; do
    if [ ! -f "$required_file" ]; then
        echo "ERROR: expected build output not found: $required_file" >&2
        exit 1
    fi
done

DRIVER_INI_DIR="$(mktemp -d)"
cat > "$DRIVER_INI_DIR/odbcinst.ini" <<EOF
[$CANDIDATE_DRIVER_NAME]
Description=mssql-odbc candidate performance build
Driver=$CANDIDATE_DRIVER
Setup=$CANDIDATE_DRIVER
UsageCount=1

[$BASELINE_DRIVER_NAME]
Description=mssql-odbc baseline performance build
Driver=$BASELINE_DRIVER
Setup=$BASELINE_DRIVER
UsageCount=1

[$MICROSOFT_DRIVER_NAME]
Description=Microsoft ODBC Driver $MICROSOFT_VERSION performance reference
Driver=$MICROSOFT_DRIVER
Setup=$MICROSOFT_DRIVER
UsageCount=1
EOF
export ODBCSYSINI="$DRIVER_INI_DIR"

BENCH_PREFIX=()
BENCH_CPUS="${BENCH_CPUS:-${PERF_CLIENT_CPUS:-}}"
if [ -n "$BENCH_CPUS" ] && command -v taskset >/dev/null 2>&1; then
    BENCH_PREFIX=(taskset -c "$BENCH_CPUS")
    echo ">>> Pinning benchmark processes to CPUs: $BENCH_CPUS"
fi

{
    echo "candidate_commit=$(git rev-parse HEAD)"
    echo "baseline_commit=$BASELINE_COMMIT"
    echo "microsoft_driver_version=$MICROSOFT_VERSION"
    echo "microsoft_driver_package=$MICROSOFT_PACKAGE_VERSION"
    echo "microsoft_driver_path=$MICROSOFT_DRIVER"
    echo "microsoft_driver_sha256=$MICROSOFT_DRIVER_SHA256"
    echo "repetitions=$REPETITIONS"
    echo "timestamp_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    uname -a
    rustc -Vv
    cargo -V
    cmake --version
} > "$RESULTS_DIR/odbc-context.txt"

echo ">>> Creating deterministic benchmark tables..."
TABLE_CLEANUP_ARMED=1
ODBC_BENCH_DRIVER="$CANDIDATE_DRIVER_NAME" \
    ODBC_BENCH_SCENARIO="" \
    "$ADMIN_EXE" setup

run_leg() {
    # A leg contains one driver/scenario sample file; ordering is interleaved above
    # to reduce bias from machine or server drift during the run.
    local scenario="$1"
    local driver="$2"
    local output="$3"
    echo ">>> Running $scenario with $driver..."
    ODBC_BENCH_DRIVER="$driver" \
        ODBC_BENCH_SCENARIO="$scenario" \
        ODBC_BENCH_PACKET_SIZE_KEYWORD="PacketSize" \
        "${BENCH_PREFIX[@]}" "$BENCH_EXE" \
        "--benchmark_repetitions=$REPETITIONS" \
        "--benchmark_out=$output" \
        --benchmark_out_format=json
}

CANDIDATE_NARROW="$RESULTS_DIR/odbc-candidate-narrow.json"
BASELINE_NARROW="$RESULTS_DIR/odbc-baseline-narrow.json"
CANDIDATE_WIDE="$RESULTS_DIR/odbc-candidate-wide.json"
BASELINE_WIDE="$RESULTS_DIR/odbc-baseline-wide.json"
MICROSOFT_NARROW="$RESULTS_DIR/odbc-microsoft-narrow.json"
MICROSOFT_WIDE="$RESULTS_DIR/odbc-microsoft-wide.json"

run_leg narrow "$CANDIDATE_DRIVER_NAME" "$CANDIDATE_NARROW"
run_leg narrow "$MICROSOFT_DRIVER_NAME" "$MICROSOFT_NARROW"
run_leg narrow "$BASELINE_DRIVER_NAME" "$BASELINE_NARROW"
run_leg wide "$BASELINE_DRIVER_NAME" "$BASELINE_WIDE"
run_leg wide "$MICROSOFT_DRIVER_NAME" "$MICROSOFT_WIDE"
run_leg wide "$CANDIDATE_DRIVER_NAME" "$CANDIDATE_WIDE"

compare_args=(
    "$REPO_ROOT/.pipeline/scripts/compare-odbc-benchmarks.py"
    --baseline "$BASELINE_NARROW"
    --baseline "$BASELINE_WIDE"
    --candidate "$CANDIDATE_NARROW"
    --candidate "$CANDIDATE_WIDE"
    --reference "$MICROSOFT_NARROW"
    --reference "$MICROSOFT_WIDE"
    --reference-label "Microsoft ODBC $MICROSOFT_VERSION"
    --baseline-commit "$BASELINE_COMMIT"
    --reference-version "$MICROSOFT_VERSION"
    --repetitions "$REPETITIONS"
    --output-dir "$RESULTS_DIR"
    --regression-ratio "${ODBC_BENCH_REGRESSION_RATIO:-1.10}"
)
if [ "${ODBC_BENCH_FAIL_ON_REGRESSION:-0}" = "1" ]; then
    compare_args+=(--fail-on-regression)
fi
# Exit 2 means the comparison completed and the optional gate tripped. Delay that
# exit until after summary.md is echoed so failed runs remain diagnosable from logs.
set +e
python3 "${compare_args[@]}"
compare_rc=$?
set -e
if [ "$compare_rc" -ne 0 ] && [ "$compare_rc" -ne 2 ]; then
    exit "$compare_rc"
fi

echo ""
echo "===== summary.md ====="
cat "$RESULTS_DIR/summary.md"
echo "===== end summary.md ====="
echo ">>> ODBC benchmark results written to $RESULTS_DIR"
if [ "$compare_rc" -eq 2 ]; then
    exit 2
fi

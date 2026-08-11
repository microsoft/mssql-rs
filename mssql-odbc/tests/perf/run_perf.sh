#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the Rust ODBC driver and the C++ Google Benchmark suite, then run the
# same benchmark binaries against mssql-odbc and msodbcsql18 and print a
# side-by-side comparison.
#
# For Unix-like platforms (Linux, macOS) that use unixODBC.
# For Windows, see run_perf.ps1.
#
# Both drivers are selected by the `Driver={...}` keyword in the connection
# string, so the same binary produces both sides of the comparison.
#
# Prerequisites:
#   The Rust driver must be registered under --rust-driver in odbcinst.ini at a
#   path this user can overwrite, e.g. in ~/.odbcinst.ini:
#     [mssql-odbc dev]
#     Driver = /home/<user>/.odbc-dev/libmsodbcsql18.so
#
# Usage:
#   ./run_perf.sh [--uid=USER] [--pwd=PASSWORD] [--server=HOST] [--database=DB]
#                 [--bench=NAME] [--filter=REGEX] [--min-time=SECONDS]
#                 [--repetitions=N] [--skip-build] [--rust-driver=NAME]
#                 [--ref-driver=NAME] [--results-dir=PATH]
#
# Examples:
#   ./run_perf.sh --uid=odbcperf --pwd='<password>'
#   ./run_perf.sh --bench=fetch_bench --min-time=2.0 --repetitions=5
#   ./run_perf.sh --skip-build --filter='BM_Fetch.*'
#   ./run_perf.sh --ref-driver=''      # measure mssql-odbc only

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ODBC_CRATE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORKSPACE_DIR="$(cd "$ODBC_CRATE_DIR/.." && pwd)"
BUILD_DIR="$SCRIPT_DIR/build"

RUST_DRIVER="mssql-odbc dev"
REF_DRIVER="ODBC Driver 18 for SQL Server"
SERVER="localhost"
DATABASE="odbcperf"
UID_ARG=""
PWD_ARG=""
BENCH=""
FILTER=""
MIN_TIME="1.0"
REPETITIONS="3"
SKIP_BUILD=0
RESULTS_DIR="$SCRIPT_DIR/results"

ALL_BENCHES=(connect_bench exec_bench fetch_bench datatype_bench)

for arg in "$@"; do
    case "$arg" in
        --rust-driver=*) RUST_DRIVER="${arg#*=}" ;;
        --ref-driver=*)  REF_DRIVER="${arg#*=}" ;;
        --server=*)      SERVER="${arg#*=}" ;;
        --database=*)    DATABASE="${arg#*=}" ;;
        --uid=*)         UID_ARG="${arg#*=}" ;;
        --pwd=*)         PWD_ARG="${arg#*=}" ;;
        --bench=*)       BENCH="${arg#*=}" ;;
        --filter=*)      FILTER="${arg#*=}" ;;
        --min-time=*)    MIN_TIME="${arg#*=}" ;;
        --repetitions=*) REPETITIONS="${arg#*=}" ;;
        --results-dir=*) RESULTS_DIR="${arg#*=}" ;;
        --skip-build)    SKIP_BUILD=1 ;;
        -h|--help)       sed -n '2,32p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "Unknown argument: $arg" >&2; exit 2 ;;
    esac
done

if [[ -n "$BENCH" ]]; then
    BENCHES=("$BENCH")
else
    BENCHES=("${ALL_BENCHES[@]}")
fi

mkdir -p "$RESULTS_DIR"

section() { printf '\n=== %s ===\n' "$1"; }

# ----------------------------------------------------------------------------
# Resolve where the registered Rust driver .so lives so a fresh build can be
# dropped in place without touching odbcinst.ini.
# ----------------------------------------------------------------------------
registered_driver_path() {
    local name="$1"
    local ini
    for ini in "$HOME/.odbcinst.ini" /etc/odbcinst.ini /usr/local/etc/odbcinst.ini; do
        [[ -f "$ini" ]] || continue
        awk -v section="[$name]" '
            $0 == section { inside = 1; next }
            /^\[/         { inside = 0 }
            inside && /^[[:space:]]*[Dd]river[[:space:]]*=/ {
                sub(/^[^=]*=[[:space:]]*/, ""); print; exit
            }
        ' "$ini" | head -n1
    done | head -n1
}

RUST_DRIVER_PATH="$(registered_driver_path "$RUST_DRIVER")"
if [[ -z "$RUST_DRIVER_PATH" ]]; then
    echo "Driver '$RUST_DRIVER' is not registered in any odbcinst.ini." >&2
    echo "See the header of this script for the one-time registration steps." >&2
    exit 1
fi

# ----------------------------------------------------------------------------
# Build
# ----------------------------------------------------------------------------
if [[ $SKIP_BUILD -eq 0 ]]; then
    section "Building Rust driver (release)"
    (cd "$ODBC_CRATE_DIR" && cargo build --release)

    BUILT_LIB=""
    for candidate in \
        "$WORKSPACE_DIR/target/release/libmsodbcsql18.so" \
        "$WORKSPACE_DIR/target/release/libmsodbcsql18.dylib"; do
        [[ -f "$candidate" ]] && BUILT_LIB="$candidate" && break
    done
    if [[ -z "$BUILT_LIB" ]]; then
        echo "Built driver not found under $WORKSPACE_DIR/target/release" >&2
        exit 1
    fi

    echo "[  DRIVER ] Copying $BUILT_LIB -> $RUST_DRIVER_PATH"
    mkdir -p "$(dirname "$RUST_DRIVER_PATH")"
    cp -f "$BUILT_LIB" "$RUST_DRIVER_PATH"

    section "Building C++ benchmarks (Release)"
    cmake -S "$SCRIPT_DIR" -B "$BUILD_DIR" -DCMAKE_BUILD_TYPE=Release
    cmake --build "$BUILD_DIR" -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
fi

# ----------------------------------------------------------------------------
# Run one benchmark binary against one driver
# ----------------------------------------------------------------------------
run_bench() {
    local bench_name="$1" driver_name="$2" label="$3"
    local exe="$BUILD_DIR/$bench_name"
    if [[ ! -x "$exe" ]]; then
        echo "Benchmark binary not found: $exe" >&2
        exit 1
    fi

    local out="$RESULTS_DIR/$bench_name.$label.json"
    local args=(
        "--benchmark_out=$out"
        --benchmark_out_format=json
        "--benchmark_min_time=${MIN_TIME}s"
        "--benchmark_repetitions=$REPETITIONS"
        --benchmark_report_aggregates_only=true
    )
    [[ -n "$FILTER" ]] && args+=("--benchmark_filter=$FILTER")

    echo "[   RUN   ] $bench_name  driver='$driver_name'"
    ODBC_TEST_DRIVER="$driver_name" \
    ODBC_TEST_SERVER="$SERVER" \
    ODBC_TEST_DATABASE="$DATABASE" \
    ODBC_TEST_UID="$UID_ARG" \
    ODBC_TEST_PWD="$PWD_ARG" \
        "$exe" "${args[@]}" || echo "[  FAIL   ] $bench_name exited $? for '$driver_name'"
}

# ----------------------------------------------------------------------------
# Compare two result files. Emits "name<TAB>rust_ns<TAB>ref_ns" lines; a failed
# case reports -1 so it stays visible in the table instead of disappearing.
# ----------------------------------------------------------------------------
compare_results() {
    local rust_json="$1" ref_json="$2"
    [[ -f "$rust_json" && -f "$ref_json" ]] || return 0
    command -v python3 >/dev/null 2>&1 || return 0

    python3 - "$rust_json" "$ref_json" <<'PY'
import json, sys

SCALE = {"ns": 1, "us": 1e3, "ms": 1e6, "s": 1e9}

def medians(path):
    out = {}
    with open(path) as fh:
        data = json.load(fh)
    for b in data.get("benchmarks", []):
        name = b["name"]
        for suffix in ("_median", "_mean", "_stddev", "_cv"):
            if name.endswith(suffix):
                name = name[: -len(suffix)]
                break
        if b.get("error_occurred"):
            out[name] = None
            continue
        if b.get("run_type") == "aggregate" and b.get("aggregate_name") != "median":
            continue
        if out.get(name, False) is None:
            continue
        out[name] = b["real_time"] * SCALE.get(b.get("time_unit", "ns"), 1)
    return out

rust, ref = medians(sys.argv[1]), medians(sys.argv[2])
for name, r in rust.items():
    if name not in ref:
        continue
    m = ref[name]
    print("%s\t%s\t%s" % (name, -1 if r is None else r, -1 if m is None else m))
PY
}

format_table() {
    awk -F'\t' '
        function fmt(ns) {
            if (ns < 0)      return "ERROR"
            if (ns >= 1e6)   return sprintf("%.2f ms", ns / 1e6)
            if (ns >= 1e3)   return sprintf("%.2f us", ns / 1e3)
            return sprintf("%.2f ns", ns)
        }
        {
            r = $2 + 0; m = $3 + 0
            if (r < 0 || m <= 0) {
                ratio = "-"; key = 1e18
                verdict = (r < 0) ? "unsupported" : "n/a"
            } else {
                key = r / m
                ratio = sprintf("%.2f", key)
                verdict = (r < m * 0.95) ? "faster" : (r > m * 1.05) ? "slower" : "parity"
            }
            printf "%.9f\t%s|%s|%s|%s|%s\n", key, $1, fmt(r), fmt(m), ratio, verdict
        }
    ' | sort -k1,1g | cut -f2- | {
        printf 'Benchmark|mssql-odbc|msodbcsql18|Ratio|Verdict\n'
        cat
    } | column -t -s'|'
}

# ----------------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------------
section "Configuration"
echo "  Rust driver : $RUST_DRIVER  ($RUST_DRIVER_PATH)"
[[ -n "$REF_DRIVER" ]] && echo "  Reference   : $REF_DRIVER"
echo "  Server      : $SERVER / $DATABASE"
if [[ -n "$UID_ARG" ]]; then
    echo "  Auth        : SQL login '$UID_ARG'"
else
    echo "  Auth        : integrated"
fi
echo "  Benchmarks  : ${BENCHES[*]}"

ROWS_FILE="$(mktemp)"
trap 'rm -f "$ROWS_FILE"' EXIT

for b in "${BENCHES[@]}"; do
    section "Benchmark: $b"
    run_bench "$b" "$RUST_DRIVER" "mssql-odbc"
    if [[ -n "$REF_DRIVER" ]]; then
        run_bench "$b" "$REF_DRIVER" "msodbcsql18"
        compare_results "$RESULTS_DIR/$b.mssql-odbc.json" \
                        "$RESULTS_DIR/$b.msodbcsql18.json" >> "$ROWS_FILE"
    fi
done

if [[ -s "$ROWS_FILE" ]]; then
    section "Comparison (median real time; Ratio < 1.0 = mssql-odbc faster)"
    format_table < "$ROWS_FILE"
fi

echo
echo "Raw JSON results in: $RESULTS_DIR"

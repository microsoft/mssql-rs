#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Dedicated Linux perf-lab runner for the mssql-odbc result-set benchmarks.

set -euo pipefail
set -E
export LC_ALL=C
trap 'rc=$?; echo "ERROR: ${BASH_SOURCE[0]}:${LINENO}: ${BASH_COMMAND} exited ${rc}" >&2' ERR

REPO_ROOT="$(pwd)"
RESULTS_DIR="$REPO_ROOT/results"
INITIAL_DIR="$RESULTS_DIR/initial"
CONFIRM_DIR="$RESULTS_DIR/confirm"
PLAN_FILE="$RESULTS_DIR/confirm-plan.txt"
TELEMETRY_CSV="$RESULTS_DIR/cpu-telemetry.csv"
BASELINE_FILE="$REPO_ROOT/mssql-odbc-bench/perf-lab/baseline-commit.txt"
REFERENCE_VERSION_FILE="$REPO_ROOT/mssql-odbc-bench/perf-lab/msodbcsql-version.txt"
# One shared snapshot query for both perf labs; do not fork a second copy.
SQL_CONFIG_SQL="$REPO_ROOT/mssql-tds-bench/perf-lab/sql-config-dump.sql"
COMPARE_SCRIPT="$REPO_ROOT/.pipeline/scripts/compare-odbc-benchmarks.py"
HARNESS_BUILD_DIR="$REPO_ROOT/target/odbc-bench"
CANDIDATE_TARGET_DIR="$REPO_ROOT/target/odbc-candidate"
BASELINE_TARGET_DIR="$REPO_ROOT/target/odbc-baseline"
CANDIDATE_DRIVER_NAME="MSSQL Rust ODBC Perf Candidate"
BASELINE_DRIVER_NAME="MSSQL Rust ODBC Perf Baseline"
MICROSOFT_DRIVER_NAME="ODBC Driver 18 for SQL Server"
NUMPY_VERSION="2.2.6"
SCIPY_VERSION="1.15.3"

# Scenario catalog. The C++ harness filters its workloads by scenario, and every
# downstream step - ordering, comparison, confirmation - iterates this list, so a
# new scenario needs no other change here.
SCENARIOS=(narrow wide rowset varwidth getdata)

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
        if ! ODBCSYSINI="$DRIVER_INI_DIR" \
            ODBC_BENCH_DRIVER="$CANDIDATE_DRIVER_NAME" \
            ODBC_BENCH_SCENARIO="" \
            "$ADMIN_EXE" cleanup; then
            echo "WARNING: candidate cleanup failed; retrying with Microsoft ODBC" >&2
            ODBCSYSINI="$DRIVER_INI_DIR" \
                ODBC_BENCH_DRIVER="$MICROSOFT_DRIVER_NAME" \
                ODBC_BENCH_SCENARIO="" \
                ODBC_BENCH_PACKET_SIZE_KEYWORD="PacketSize" \
                "$ADMIN_EXE" cleanup ||
                echo "WARNING: benchmark table cleanup failed with both drivers" >&2
        fi
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

# Bracketed CPU frequency/utilization samples around every measured pass. If a
# confirmation round disagrees with the initial pass, this is what says whether
# the machine changed underneath the measurement or the driver did.
echo "timestamp,label,avg_cur_freq_mhz,cpu_busy_pct,temp_c" > "$TELEMETRY_CSV"
cpu_busy_percent() {
    # /proc/stat is cumulative since boot, so a single read reports a lifetime
    # average. Take a short delta instead so the number describes this pass.
    local first second
    first=$(awk '/^cpu /{ t = 0; for (i = 2; i <= NF; i++) t += $i; print t, $5; exit }' /proc/stat) ||
        return 1
    sleep 0.2
    second=$(awk '/^cpu /{ t = 0; for (i = 2; i <= NF; i++) t += $i; print t, $5; exit }' /proc/stat) ||
        return 1
    awk -v a="$first" -v b="$second" 'BEGIN {
        split(a, before, " ");
        split(b, after, " ");
        total = after[1] - before[1];
        idle = after[2] - before[2];
        if (total <= 0) exit 1;
        printf "%.1f", (total - idle) * 100 / total
    }'
}

cpu_sample() {
    local label="$1" sum=0 count=0 file value freq_mhz="" busy="" zone reading temp_c=""
    for file in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_cur_freq; do
        [ -r "$file" ] || continue
        value=$(cat "$file" 2>/dev/null) || continue
        sum=$((sum + value))
        count=$((count + 1))
    done
    if [ "$count" -gt 0 ]; then
        freq_mhz=$((sum / count / 1000))
    fi
    if [ -r /proc/stat ]; then
        busy=$(cpu_busy_percent 2>/dev/null) || busy=""
    fi
    for zone in /sys/class/thermal/thermal_zone*/temp; do
        [ -r "$zone" ] || continue
        reading=$(cat "$zone" 2>/dev/null) || continue
        temp_c=$((reading / 1000))
        break
    done
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ),${label},${freq_mhz},${busy},${temp_c}" \
        >> "$TELEMETRY_CSV"
    echo ">>> cpu[${label}] avgFreq=${freq_mhz}MHz busy=${busy}% temp=${temp_c}C"
}

: "${SQL_SERVER:?SQL_SERVER not set}"
: "${SQL_PASSWORD:?SQL_PASSWORD not set}"

export ODBC_BENCH_SERVER="${ODBC_BENCH_SERVER:-$SQL_SERVER}"
export ODBC_BENCH_DATABASE="${ODBC_BENCH_DATABASE:-tempdb}"
export ODBC_BENCH_UID="${ODBC_BENCH_UID:-${DB_USERNAME:-sa}}"
export ODBC_BENCH_PWD="${ODBC_BENCH_PWD:-$SQL_PASSWORD}"
export ODBC_BENCH_TRUST_CERT="${ODBC_BENCH_TRUST_CERT:-Yes}"
export ODBC_BENCH_ENCRYPT="${ODBC_BENCH_ENCRYPT:-Mandatory}"
export ODBC_BENCH_PACKET_SIZE="${ODBC_BENCH_PACKET_SIZE:-16192}"

# --- Allocator tuning (steadier large rowset buffers) ---
# Each retrieval allocates the bound rowset buffers for up to 600 columns by
# 1000 rows, which is far past glibc's dynamic mmap threshold. Left alone, every
# repetition re-mmaps and re-faults those pages, which is both slower and much
# noisier than the driver difference we are trying to measure. Raise the mmap
# threshold so the buffers come from the heap and stop trimming so they are
# reused. The mmap threshold must be raised explicitly: setting only the trim
# threshold disables glibc's dynamic mmap threshold and forces every large
# allocation through mmap.
export MALLOC_MMAP_THRESHOLD_="${MALLOC_MMAP_THRESHOLD_:-134217728}"  # 128 MB
export MALLOC_TRIM_THRESHOLD_="${MALLOC_TRIM_THRESHOLD_:--1}"          # never trim

# --- No connection-churn network tuning here (deliberate) ---
# mssql-tds-bench widens the ephemeral port range and enables TIME_WAIT reuse
# because its concurrent_connects benchmark opens tens of thousands of
# short-lived TCP connections. This harness opens ONE connection in OdbcSession,
# holds it for the whole process, and measures only statement execution and
# fetching, so there is no port pressure to relieve. The CPU/server diagnostics
# and the large-buffer allocator control above do apply and are enabled.

read_positive_int() {
    # Parse tuning knobs once, in base 10, so "08" is neither an octal literal
    # error nor silently a different number than the PowerShell runner reads.
    local name="$1" value="$2" minimum="$3"
    case "$value" in
        ''|*[!0-9]*)
            echo "ERROR: $name must be a positive integer (got: '$value')" >&2
            exit 1
            ;;
    esac
    value=$((10#$value))
    if [ "$value" -lt "$minimum" ]; then
        echo "ERROR: $name must be >= $minimum (got: $value)" >&2
        exit 1
    fi
    printf '%s' "$value"
}

REPETITIONS="$(read_positive_int ODBC_BENCH_REPETITIONS "${ODBC_BENCH_REPETITIONS:-5}" 1)"
# Confirmation defaults match the fixed-baseline mssql-tds runner: four targeted
# re-runs, reproduction required in a majority (3 of 4).
CONFIRM_RUNS="$(read_positive_int ODBC_BENCH_CONFIRM_RUNS "${ODBC_BENCH_CONFIRM_RUNS:-4}" 1)"
CONFIRM_QUORUM="$(
    read_positive_int ODBC_BENCH_CONFIRM_QUORUM \
        "${ODBC_BENCH_CONFIRM_QUORUM:-$((CONFIRM_RUNS / 2 + 1))}" 1
)"
if [ "$CONFIRM_QUORUM" -gt "$CONFIRM_RUNS" ]; then
    echo "ERROR: ODBC_BENCH_CONFIRM_QUORUM must not exceed ODBC_BENCH_CONFIRM_RUNS" >&2
    exit 1
fi
IMPROVEMENT_MAX="$(
    read_positive_int ODBC_BENCH_IMPROVEMENT_VERIFY_MAX \
        "${ODBC_BENCH_IMPROVEMENT_VERIFY_MAX:-3}" 1
)"
REGRESSION_RATIO="${ODBC_BENCH_REGRESSION_RATIO:-1.05}"
IMPROVEMENT_RATIO="${ODBC_BENCH_IMPROVEMENT_VERIFY_RATIO:-$REGRESSION_RATIO}"
# A confirmed candidate-vs-pinned-baseline regression fails the run by default;
# set ODBC_BENCH_FAIL_ON_REGRESSION=0 to publish the report without gating.
FAIL_ON_REGRESSION="${ODBC_BENCH_FAIL_ON_REGRESSION:-1}"

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
    # Google Benchmark's comparator imports NumPy and SciPy unconditionally, so a
    # private virtualenv needs to be creatable even when it is never used.
    python3 -c 'import ensurepip, venv' >/dev/null 2>&1 || missing+=(python3-venv)
    [ ${#missing[@]} -eq 0 ] && return

    local sudo_command=()
    [ "$(id -u)" -ne 0 ] && sudo_command=(sudo)
    echo ">>> Installing system packages: ${missing[*]}"
    "${sudo_command[@]}" apt-get update -y
    "${sudo_command[@]}" env DEBIAN_FRONTEND=noninteractive \
        apt-get install -y --no-install-recommends "${missing[@]}"
}
ensure_packages

BENCH_PYTHON="$(command -v python3)"
ensure_python_stats() {
    # gbench/report.py imports numpy and scipy at module scope even with
    # --no-utest, so the official comparator cannot run without them. Prefer an
    # interpreter that already has them; otherwise build a private virtualenv so
    # nothing is installed into the perf host's system Python.
    if "$BENCH_PYTHON" -c 'import numpy, scipy' >/dev/null 2>&1; then
        return 0
    fi
    local venv="$REPO_ROOT/target/odbc-bench-venv"
    echo ">>> Provisioning NumPy/SciPy for Google Benchmark's comparator..."
    if [ ! -x "$venv/bin/python" ] && ! python3 -m venv "$venv"; then
        return 1
    fi
    "$venv/bin/python" -m pip install --quiet --upgrade pip >/dev/null 2>&1 || true
    "$venv/bin/python" -m pip install --quiet \
        "numpy==$NUMPY_VERSION" "scipy==$SCIPY_VERSION" || return 1
    BENCH_PYTHON="$venv/bin/python"
}

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

# --- SQL Server configuration snapshot (validate the instance is tuned) ---
# Memory, MAXDOP, cost threshold, affinity, tempdb placement, recovery, and
# trace flags. Best-effort: sqlcmd is not guaranteed on a perf image, and a
# missing snapshot must not cost a whole lab run.
sqlcmd_bin="$(command -v sqlcmd || true)"
if [ -z "$sqlcmd_bin" ] && [ -x /opt/mssql-tools18/bin/sqlcmd ]; then
    sqlcmd_bin=/opt/mssql-tools18/bin/sqlcmd
fi
if [ -z "$sqlcmd_bin" ] && [ -x /opt/mssql-tools/bin/sqlcmd ]; then
    sqlcmd_bin=/opt/mssql-tools/bin/sqlcmd
fi
if [ -n "$sqlcmd_bin" ] && [ -f "$SQL_CONFIG_SQL" ]; then
    echo ">>> Capturing SQL Server configuration snapshot..."
    "$sqlcmd_bin" -S "$ODBC_BENCH_SERVER" -U "$ODBC_BENCH_UID" -P "$ODBC_BENCH_PWD" \
        -C -b -y 0 -Y 30 -i "$SQL_CONFIG_SQL" |
        tee "$RESULTS_DIR/sql-config.txt" ||
        echo ">>> SQL config snapshot skipped (query failed)."
else
    echo ">>> Skipping SQL config snapshot (sqlcmd or query file not found)."
fi

echo ">>> Building the fixed C++ benchmark harness..."
cmake -S "$REPO_ROOT/mssql-odbc-bench" \
    -B "$HARNESS_BUILD_DIR" \
    -DCMAKE_BUILD_TYPE=Release
cmake --build "$HARNESS_BUILD_DIR" --config Release --parallel

# The pinned Google Benchmark v1.9.1 checkout ships the comparator we report
# with; using the copy from this build tree keeps the tool and the harness on
# the same version.
GBENCH_COMPARE="$HARNESS_BUILD_DIR/_deps/googlebenchmark-src/tools/compare.py"
if [ ! -f "$GBENCH_COMPARE" ]; then
    echo "ERROR: Google Benchmark comparator not found: $GBENCH_COMPARE" >&2
    exit 1
fi
GBENCH_ARGS=(--gbench-compare "$GBENCH_COMPARE")
if ! ensure_python_stats; then
    echo "ERROR: NumPy/SciPy are required by Google Benchmark's compare.py" >&2
    exit 1
fi

build_driver() {
    # Separate target directories keep the two Rust driver builds comparable and
    # prevent Cargo from reusing candidate artifacts for the pinned baseline.
    local source_root="$1"
    local target_dir="$2"
    local label="$3"
    echo ">>> Building $label mssql-odbc driver..."
    (
        cd "$source_root"
        CARGO_TARGET_DIR="$target_dir" cargo build \
            --manifest-path mssql-odbc/Cargo.toml --release
    )
}

driver_artifact() {
    # The pinned baseline can predate a cdylib rename, so resolve each checkout.
    local source_root="$1"
    local target_dir="$2"
    local target_name
    target_name="$(
        awk '
            /^\[lib\][[:space:]]*$/ { in_lib = 1; next }
            /^\[/ { in_lib = 0 }
            in_lib && /^[[:space:]]*name[[:space:]]*=/ {
                name = $0
                sub(/^[^"]*"/, "", name)
                sub(/".*$/, "", name)
                print name
                exit
            }
        ' "$source_root/mssql-odbc/Cargo.toml"
    )"
    if [ -z "$target_name" ]; then
        echo "ERROR: cdylib target name not found in $source_root/mssql-odbc/Cargo.toml" >&2
        return 1
    fi
    printf '%s/release/lib%s.so\n' "$target_dir" "$target_name"
}

build_driver "$REPO_ROOT" "$CANDIDATE_TARGET_DIR" "candidate"

BASELINE_TEMP_DIR="$(mktemp -d)"
BASELINE_TREE="$BASELINE_TEMP_DIR/worktree"
echo ">>> Adding baseline worktree for $BASELINE_COMMIT..."
git worktree add --detach "$BASELINE_TREE" "$BASELINE_COMMIT"
build_driver "$BASELINE_TREE" "$BASELINE_TARGET_DIR" "baseline"

CANDIDATE_DRIVER="$(driver_artifact "$REPO_ROOT" "$CANDIDATE_TARGET_DIR")"
BASELINE_DRIVER="$(driver_artifact "$BASELINE_TREE" "$BASELINE_TARGET_DIR")"
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
    echo "packet_size=$ODBC_BENCH_PACKET_SIZE"
    echo "packet_size_verified_by_harness=true"
    echo "repetitions=$REPETITIONS"
    echo "regression_ratio=$REGRESSION_RATIO"
    echo "confirm_runs=$CONFIRM_RUNS"
    echo "confirm_quorum=$CONFIRM_QUORUM"
    echo "gbench_compare=${GBENCH_ARGS[1]}"
    echo "numpy_version=$NUMPY_VERSION"
    echo "scipy_version=$SCIPY_VERSION"
    echo "bench_python=$BENCH_PYTHON"
    echo "malloc_mmap_threshold=$MALLOC_MMAP_THRESHOLD_"
    echo "malloc_trim_threshold=$MALLOC_TRIM_THRESHOLD_"
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
    # A leg contains one driver/scenario sample file; ordering is interleaved
    # below to reduce bias from machine or server drift during the run.
    local scenario="$1"
    local driver="$2"
    local output="$3"
    echo ">>> Running $scenario with $driver..."
    # Linux keeps the PacketSize spelling for every driver, including Microsoft
    # ODBC: on Linux that driver rejects "Packet Size" (01S00) and accepts
    # "PacketSize" (01S02). Windows uses the "Packet Size" spelling instead.
    ODBC_BENCH_DRIVER="$driver" \
        ODBC_BENCH_SCENARIO="$scenario" \
        ODBC_BENCH_PACKET_SIZE_KEYWORD="PacketSize" \
        "${BENCH_PREFIX[@]}" "$BENCH_EXE" \
        "--benchmark_repetitions=$REPETITIONS" \
        "--benchmark_out=$output" \
        --benchmark_out_format=json
}

CANDIDATE_FILES=()
BASELINE_FILES=()
MICROSOFT_FILES=()
for scenario in "${SCENARIOS[@]}"; do
    CANDIDATE_FILES+=("$RESULTS_DIR/odbc-candidate-$scenario.json")
    BASELINE_FILES+=("$RESULTS_DIR/odbc-baseline-$scenario.json")
    MICROSOFT_FILES+=("$RESULTS_DIR/odbc-microsoft-$scenario.json")
done

cpu_sample "initial-start"
for index in "${!SCENARIOS[@]}"; do
    scenario="${SCENARIOS[$index]}"
    # Alternate which Rust driver runs first per scenario, with Microsoft ODBC
    # between them, so a stable position effect does not favour one side.
    if [ $((index % 2)) -eq 0 ]; then
        run_leg "$scenario" "$CANDIDATE_DRIVER_NAME" "${CANDIDATE_FILES[$index]}"
        run_leg "$scenario" "$MICROSOFT_DRIVER_NAME" "${MICROSOFT_FILES[$index]}"
        run_leg "$scenario" "$BASELINE_DRIVER_NAME" "${BASELINE_FILES[$index]}"
    else
        run_leg "$scenario" "$BASELINE_DRIVER_NAME" "${BASELINE_FILES[$index]}"
        run_leg "$scenario" "$MICROSOFT_DRIVER_NAME" "${MICROSOFT_FILES[$index]}"
        run_leg "$scenario" "$CANDIDATE_DRIVER_NAME" "${CANDIDATE_FILES[$index]}"
    fi
done
cpu_sample "initial-end"

three_way_args=()
for index in "${!SCENARIOS[@]}"; do
    three_way_args+=(--baseline "${BASELINE_FILES[$index]}")
done
for index in "${!SCENARIOS[@]}"; do
    three_way_args+=(--candidate "${CANDIDATE_FILES[$index]}")
done
for index in "${!SCENARIOS[@]}"; do
    three_way_args+=(--reference "${MICROSOFT_FILES[$index]}")
done
three_way_args+=(
    --reference-label "Microsoft ODBC $MICROSOFT_VERSION"
    --baseline-commit "$BASELINE_COMMIT"
    --reference-version "$MICROSOFT_VERSION"
    --repetitions "$REPETITIONS"
    --regression-ratio "$REGRESSION_RATIO"
    --improvement-ratio "$IMPROVEMENT_RATIO"
    --improvement-max "$IMPROVEMENT_MAX"
    "${GBENCH_ARGS[@]}"
)

# --- Initial pass: five-sample medians pick what deserves re-measurement ---
# The initial verdict never gates on its own; it only produces the plan.
echo ">>> Comparing the initial three-driver pass..."
"$BENCH_PYTHON" "$COMPARE_SCRIPT" \
    "${three_way_args[@]}" \
    --output-dir "$INITIAL_DIR" \
    --plan-out "$PLAN_FILE" \
    --no-summary \
    --no-fail-on-regression

scenario_for_benchmark() {
    # Confirmation re-runs whole scenarios, not single benchmarks, so each
    # flagged id has to be mapped back to the leg it came out of.
    local id="$1"
    local index
    for index in "${!SCENARIOS[@]}"; do
        if grep -qF "\"run_name\": \"$id\"" "${CANDIDATE_FILES[$index]}"; then
            printf '%s' "${SCENARIOS[$index]}"
            return 0
        fi
    done
    echo "ERROR: cannot map benchmark '$id' to a scenario" >&2
    return 1
}

VERIFY_NAMES=()
VERIFY_KINDS=()
CONFIRM_SCENARIOS=()
while IFS=$'\t' read -r plan_kind plan_name plan_ratio; do
    [ -n "${plan_name:-}" ] || continue
    VERIFY_KINDS+=("$plan_kind")
    VERIFY_NAMES+=("$plan_name")
    echo ">>> Initial pass flagged $plan_kind: $plan_name (ratio $plan_ratio)"
    plan_scenario="$(scenario_for_benchmark "$plan_name")"
    already_selected=0
    for selected in ${CONFIRM_SCENARIOS[@]+"${CONFIRM_SCENARIOS[@]}"}; do
        [ "$selected" = "$plan_scenario" ] && already_selected=1
    done
    [ "$already_selected" -eq 0 ] && CONFIRM_SCENARIOS+=("$plan_scenario")
done < "$PLAN_FILE"

CONFIRMATION_ARGS=()
if [ "${#VERIFY_NAMES[@]}" -gt 0 ]; then
    echo ">>> Auto-confirm: re-measuring ${#VERIFY_NAMES[@]} benchmark(s) across" \
        "${#CONFIRM_SCENARIOS[@]} scenario(s) over $CONFIRM_RUNS round(s); a result" \
        "counts only when it reproduces in >= $CONFIRM_QUORUM of $CONFIRM_RUNS."
    for round in $(seq 1 "$CONFIRM_RUNS"); do
        round_dir="$CONFIRM_DIR/round$round"
        mkdir -p "$round_dir"
        echo ">>> Confirmation round $round/$CONFIRM_RUNS..."
        cpu_sample "confirm${round}-start"
        round_args=()
        for scenario in "${CONFIRM_SCENARIOS[@]}"; do
            # Keep each pair adjacent, and alternate which side runs first so a
            # stable position effect cancels across the default four rounds.
            if [ $((round % 2)) -eq 1 ]; then
                run_leg "$scenario" "$CANDIDATE_DRIVER_NAME" \
                    "$round_dir/candidate-$scenario.json"
                run_leg "$scenario" "$BASELINE_DRIVER_NAME" \
                    "$round_dir/baseline-$scenario.json"
            else
                run_leg "$scenario" "$BASELINE_DRIVER_NAME" \
                    "$round_dir/baseline-$scenario.json"
                run_leg "$scenario" "$CANDIDATE_DRIVER_NAME" \
                    "$round_dir/candidate-$scenario.json"
            fi
            round_args+=(
                --baseline "$round_dir/baseline-$scenario.json"
                --candidate "$round_dir/candidate-$scenario.json"
            )
        done
        cpu_sample "confirm${round}-end"
        "$BENCH_PYTHON" "$COMPARE_SCRIPT" \
            "${round_args[@]}" \
            --repetitions "$REPETITIONS" \
            --regression-ratio "$REGRESSION_RATIO" \
            --improvement-ratio "$IMPROVEMENT_RATIO" \
            --output-dir "$round_dir" \
            --ratios-out "$round_dir/ratios.txt" \
            --no-summary \
            --no-fail-on-regression \
            "${GBENCH_ARGS[@]}" > "$round_dir/comparison.log"
    done

    for index in "${!VERIFY_NAMES[@]}"; do
        name="${VERIFY_NAMES[$index]}"
        kind="${VERIFY_KINDS[$index]}"
        ratios=()
        for round in $(seq 1 "$CONFIRM_RUNS"); do
            value="$(
                awk -F'\t' -v id="$name" '$1 == id { print $2; exit }' \
                    "$CONFIRM_DIR/round$round/ratios.txt"
            )"
            [ -n "$value" ] && ratios+=("$value")
        done
        if [ "${#ratios[@]}" -ne "$CONFIRM_RUNS" ]; then
            echo "ERROR: expected $CONFIRM_RUNS confirmation ratios for '$name';" \
                "found ${#ratios[@]}" >&2
            exit 1
        fi
        hits="$(
            printf '%s\n' "${ratios[@]}" |
                awk -v thr="$REGRESSION_RATIO" -v imp="$IMPROVEMENT_RATIO" -v kind="$kind" '
                    kind == "regression" { if ($1 + 0 >= thr) count++; next }
                    { if ($1 + 0 <= 1 / imp) count++ }
                    END { print count + 0 }'
        )"
        regression_hits="$(
            printf '%s\n' "${ratios[@]}" |
                awk -v thr="$REGRESSION_RATIO" '
                    $1 + 0 >= thr { count++ }
                    END { print count + 0 }'
        )"
        # Median of the confirmation rounds only. The initial pass is excluded on
        # purpose: a benchmark is re-measured because that pass was extreme, so
        # letting it vote again would let the outlier under test decide its own
        # verdict and could contradict a quorum that cleared it.
        median="$(
            printf '%s\n' "${ratios[@]}" | sort -n |
                awk '{ v[NR] = $1 + 0 }
                     END { if (NR % 2) printf "%.6f\n", v[(NR + 1) / 2];
                           else printf "%.6f\n", (v[NR / 2] + v[NR / 2 + 1]) / 2 }'
        )"
        echo ">>> $name: reproduced $hits/$CONFIRM_RUNS in the initial direction," \
            "regressed $regression_hits/$CONFIRM_RUNS, confirmation median $median"
        CONFIRMATION_ARGS+=(
            --confirmation "$name" "$hits" "$regression_hits" "$median"
        )
    done
fi

final_args=(
    "${three_way_args[@]}"
    --output-dir "$RESULTS_DIR"
    --confirm-runs "$CONFIRM_RUNS"
    --confirm-quorum "$CONFIRM_QUORUM"
    "${CONFIRMATION_ARGS[@]}"
)
if [ "$FAIL_ON_REGRESSION" = "0" ]; then
    final_args+=(--no-fail-on-regression)
fi
# Exit 2 means the comparison completed and the gate tripped. Delay that exit
# until after summary.md is echoed so failed runs remain diagnosable from logs.
set +e
"$BENCH_PYTHON" "$COMPARE_SCRIPT" "${final_args[@]}"
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

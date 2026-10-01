#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# run-benchmarks.sh — perf-lab testScript: ConnectorX TPC-H A/B, Tiberius vs mssql-tds.
#
# Runs on the dedicated perf VM (SQL Server 2022 colocated, pinned to the lower
# half of the cores). The Perf.Test.Job template copies the repo to ~/perf-tests,
# injects SQL_SERVER / SQL_PASSWORD / PERF_CLIENT_CPUS, and passes any
# testScriptArgs through as KEY=VALUE arguments.
#
# Arms (each read runs in a fresh, CPU-pinned Python process):
#   baseline            connectorx==$CX_BASELINE  — last release before mssql-tds (Tiberius only)
#   candidate           connectorx==$CX_CANDIDATE — default driver (mssql-tds)
#   candidate-tiberius  connectorx==$CX_CANDIDATE with cx.mssql_driver = "tiberius";
#                       only with INCLUDE_CONTROL=1. Separates the driver from other
#                       changes between the two releases.
set -euo pipefail
set -E
trap 'rc=$?; echo "ERROR: ${BASH_SOURCE[0]}:${LINENO}: \`${BASH_COMMAND}\` exited ${rc}" >&2' ERR

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR="$HERE/results"
mkdir -p "$RESULTS_DIR"

SF=10
ROUNDS=5
WARMUP_ROUNDS=1
PARTITIONS=1,4
ENCRYPT_MODES=false,true
RETURN_TYPES=arrow
CX_BASELINE=0.4.6
CX_CANDIDATE=0.4.7a1
INCLUDE_CONTROL=0
for kv in "$@"; do
    case "$kv" in
        SF=*|ROUNDS=*|WARMUP_ROUNDS=*|PARTITIONS=*|ENCRYPT_MODES=*|RETURN_TYPES=*|CX_BASELINE=*|CX_CANDIDATE=*|INCLUDE_CONTROL=*)
            declare "$kv" ;;
        *) echo "ERROR: unknown argument '$kv'" >&2; exit 2 ;;
    esac
done
check() { [[ "$2" =~ $3 ]] || { echo "ERROR: invalid $1='$2'" >&2; exit 2; }; }
check SF "$SF" '^[0-9]+(\.[0-9]+)?$'
check ROUNDS "$ROUNDS" '^[0-9]+$'
check WARMUP_ROUNDS "$WARMUP_ROUNDS" '^[0-9]+$'
check PARTITIONS "$PARTITIONS" '^[0-9]+(,[0-9]+)*$'
check ENCRYPT_MODES "$ENCRYPT_MODES" '^(true|false)(,(true|false))*$'
check RETURN_TYPES "$RETURN_TYPES" '^(arrow|pandas)(,(arrow|pandas))*$'
check CX_BASELINE "$CX_BASELINE" '^[0-9A-Za-z.+-]+$'
check CX_CANDIDATE "$CX_CANDIDATE" '^[0-9A-Za-z.+-]+$'
check INCLUDE_CONTROL "$INCLUDE_CONTROL" '^[01]$'

: "${SQL_SERVER:?SQL_SERVER not set}"
: "${SQL_PASSWORD:?SQL_PASSWORD not set}"
DB_USER="${DB_USERNAME:-sa}"
DB_NAME=tpch

echo ">>> SF=$SF ROUNDS=$ROUNDS WARMUP_ROUNDS=$WARMUP_ROUNDS PARTITIONS=$PARTITIONS ENCRYPT_MODES=$ENCRYPT_MODES RETURN_TYPES=$RETURN_TYPES"
echo ">>> connectorx baseline=$CX_BASELINE candidate=$CX_CANDIDATE include_control=$INCLUDE_CONTROL"

SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

# --- System prerequisites ---
missing=()
command -v python3 >/dev/null 2>&1 || missing+=(python3)
python3 -c 'import ensurepip, venv' >/dev/null 2>&1 || missing+=(python3-venv)
if [ ${#missing[@]} -gt 0 ]; then
    $SUDO apt-get update -y
    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${missing[@]}"
fi
TOOLS=/opt/mssql-tools18/bin
if [ ! -x "$TOOLS/bcp" ]; then
    # The lab image installs mssql-tools18 at provision time; this is a fallback.
    $SUDO env DEBIAN_FRONTEND=noninteractive ACCEPT_EULA=Y apt-get install -y --no-install-recommends mssql-tools18
fi
sqlq() { "$TOOLS/sqlcmd" -S "$SQL_SERVER" -U "$DB_USER" -P "$SQL_PASSWORD" -C -b -h -1 -W "$@"; }

# --- Python environments: one per ConnectorX release, identical companions ---
VENVS="$HOME/cx-venvs"
COMMON_PKGS=(pyarrow==21.0.0 pandas==2.2.3 numpy==2.2.6)
make_venv() {
    local dir="$1"; shift
    python3 -m venv "$dir"
    "$dir/bin/python" -m pip install --quiet --upgrade pip
    "$dir/bin/python" -m pip install --quiet "$@"
}
echo ">>> Creating virtualenvs..."
make_venv "$VENVS/tools" tpchgen-cli==3.0.0
make_venv "$VENVS/baseline" "connectorx==$CX_BASELINE" "${COMMON_PKGS[@]}"
make_venv "$VENVS/candidate" "connectorx==$CX_CANDIDATE" "${COMMON_PKGS[@]}"
if [ "$INCLUDE_CONTROL" = 1 ] && ! "$VENVS/candidate/bin/python" -c 'import connectorx as cx; assert hasattr(cx, "mssql_driver")'; then
    echo "ERROR: connectorx==$CX_CANDIDATE has no runtime mssql_driver switch; cannot build the control arm." >&2
    exit 1
fi

# --- Storage: the VM's local temp SSD when present (tempdb already lives there) ---
if mountpoint -q /mnt; then
    DATA_ROOT=/mnt
else
    DATA_ROOT="$HOME"
fi
TBL_DIR="$DATA_ROOT/tpch-sf$SF"
$SUDO install -d -o "$(id -un)" "$TBL_DIR"
DB_DIR=""
if [ "$DATA_ROOT" = /mnt ] && id mssql >/dev/null 2>&1; then
    DB_DIR=/mnt/tpchdb
    $SUDO install -d -o mssql -g mssql -m 750 "$DB_DIR"
fi

# --- Generate and load TPC-H lineitem ---
echo ">>> Generating TPC-H lineitem SF$SF..."
"$VENVS/tools/bin/tpchgen-cli" -s "$SF" --tables lineitem --output-dir "$TBL_DIR" --quiet
TBL="$TBL_DIR/lineitem.tbl"
EXPECTED_ROWS="$(wc -l < "$TBL" | tr -d ' ')"
echo ">>> Generated $EXPECTED_ROWS rows ($(du -h "$TBL" | cut -f1))."

if [ -n "$DB_DIR" ]; then
    DB_FILES="ON (NAME = ${DB_NAME}_data, FILENAME = '$DB_DIR/$DB_NAME.mdf', SIZE = 4GB, FILEGROWTH = 1GB)
              LOG ON (NAME = ${DB_NAME}_log, FILENAME = '$DB_DIR/${DB_NAME}_log.ldf', SIZE = 2GB, FILEGROWTH = 1GB)"
else
    DB_FILES=""
fi
echo ">>> Creating database $DB_NAME..."
sqlq -Q "
IF DB_ID('$DB_NAME') IS NOT NULL BEGIN ALTER DATABASE $DB_NAME SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE $DB_NAME; END;
CREATE DATABASE $DB_NAME $DB_FILES;
ALTER DATABASE $DB_NAME SET RECOVERY SIMPLE;
ALTER DATABASE $DB_NAME SET DELAYED_DURABILITY = FORCED;"
sqlq -d "$DB_NAME" -Q "
CREATE TABLE dbo.lineitem (
    l_orderkey      BIGINT        NOT NULL,
    l_partkey       INT           NOT NULL,
    l_suppkey       INT           NOT NULL,
    l_linenumber    INT           NOT NULL,
    l_quantity      DECIMAL(15,2) NOT NULL,
    l_extendedprice DECIMAL(15,2) NOT NULL,
    l_discount      DECIMAL(15,2) NOT NULL,
    l_tax           DECIMAL(15,2) NOT NULL,
    l_returnflag    CHAR(1)       NOT NULL,
    l_linestatus    CHAR(1)       NOT NULL,
    l_shipdate      DATE          NOT NULL,
    l_commitdate    DATE          NOT NULL,
    l_receiptdate   DATE          NOT NULL,
    l_shipinstruct  CHAR(25)      NOT NULL,
    l_shipmode      CHAR(10)      NOT NULL,
    l_comment       VARCHAR(44)   NOT NULL,
    CONSTRAINT pk_lineitem PRIMARY KEY CLUSTERED (l_orderkey, l_linenumber)
);"

# tpchgen emits rows already ordered by (orderkey, linenumber), so the ORDER hint
# lets the clustered load stream without a sort. Rows end with a trailing '|'.
echo ">>> Loading lineitem with bcp..."
load_start=$SECONDS
"$TOOLS/bcp" "dbo.lineitem" in "$TBL" -S "$SQL_SERVER" -U "$DB_USER" -P "$SQL_PASSWORD" -d "$DB_NAME" -u \
    -c -t '|' -r '|\n' -b 1000000 -h "TABLOCK,ORDER(l_orderkey ASC, l_linenumber ASC)" \
    -e "$RESULTS_DIR/bcp-errors.txt" 2>&1 | tee "$RESULTS_DIR/bcp.log"
LOADED_ROWS="$(sqlq -d "$DB_NAME" -Q "SET NOCOUNT ON; SELECT COUNT_BIG(*) FROM dbo.lineitem;" | tr -d '[:space:]')"
echo ">>> Loaded $LOADED_ROWS rows in $(( SECONDS - load_start ))s."
if [ "$LOADED_ROWS" != "$EXPECTED_ROWS" ]; then
    echo "ERROR: loaded $LOADED_ROWS rows but generated $EXPECTED_ROWS." >&2
    exit 1
fi
sqlq -d "$DB_NAME" -Q "CHECKPOINT;" >/dev/null
rm -f "$TBL"

# --- Environment snapshot ---
{
    echo "== lscpu"; lscpu
    echo; echo "== memory"; free -g
    echo; echo "== SQL Server"; sqlq -Q "SET NOCOUNT ON; SELECT @@VERSION;"
    echo; echo "== client CPUs: ${PERF_CLIENT_CPUS:-unpinned}; SQL CPUs: ${PERF_SQL_CPUS:-unknown}"
    echo; echo "== python"; python3 --version
    echo; echo "== venv baseline"; "$VENVS/baseline/bin/python" -m pip freeze
    echo; echo "== venv candidate"; "$VENVS/candidate/bin/python" -m pip freeze
} > "$RESULTS_DIR/environment.txt" 2>&1 || true

# --- Pin the client away from SQL Server's cores ---
PREFIX=()
if [ -n "${PERF_CLIENT_CPUS:-}" ] && command -v taskset >/dev/null 2>&1; then
    echo ">>> Pinning benchmark clients to CPUs ${PERF_CLIENT_CPUS}"
    PREFIX=(taskset -c "$PERF_CLIENT_CPUS")
fi

ARMS=(--arm "baseline|$VENVS/baseline/bin/python" --arm "candidate|$VENVS/candidate/bin/python")
if [ "$INCLUDE_CONTROL" = 1 ]; then
    ARMS+=(--arm "candidate-tiberius|$VENVS/candidate/bin/python|tiberius")
fi

echo ">>> Running A/B benchmark..."
rc=0
"$VENVS/candidate/bin/python" "$HERE/bench.py" run "${ARMS[@]}" \
    --host "$SQL_SERVER" --user "$DB_USER" --database "$DB_NAME" \
    --partitions "$PARTITIONS" --encrypt-modes "$ENCRYPT_MODES" --return-types "$RETURN_TYPES" \
    --rounds "$ROUNDS" --warmup-rounds "$WARMUP_ROUNDS" \
    --expected-rows "$EXPECTED_ROWS" --scale-factor "$SF" \
    --results-dir "$RESULTS_DIR" -- "${PREFIX[@]}" || rc=$?

echo "===== summary.md ====="
cat "$RESULTS_DIR/summary.md" 2>/dev/null || echo "summary.md not produced"
echo "===== end summary.md ====="
exit "$rc"

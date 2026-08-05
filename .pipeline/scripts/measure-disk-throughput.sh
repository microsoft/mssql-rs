#!/usr/bin/env bash
# Measure raw disk throughput where the agent work folder lives (Linux).
# Uses fio with direct=1 (O_DIRECT) so results reflect the disk, not page cache.
# Reports sequential + random throughput and IOPS.
set -uo pipefail

LABEL="${1:-unknown-pool}"
TARGET_DIR="${2:-}"
SIZE_GB="${3:-4}"
DURATION="${4:-20}"

section() { echo ""; echo "==== $1 ===="; }

# --- Resolve the work folder (where build I/O actually happens) ---
if [ -z "$TARGET_DIR" ]; then TARGET_DIR="${PIPELINE_WORKSPACE:-}"; fi
if [ -z "$TARGET_DIR" ]; then TARGET_DIR="${AGENT_BUILDDIRECTORY:-}"; fi
if [ -z "$TARGET_DIR" ]; then TARGET_DIR="${AGENT_WORKFOLDER:-}"; fi
if [ -z "$TARGET_DIR" ]; then TARGET_DIR="$(pwd)"; fi
mkdir -p "$TARGET_DIR"
echo "Label           : $LABEL"
echo "Work/target dir : $TARGET_DIR"

# --- Report the block device backing that directory ---
section "Disk topology"
SRC_DEV="$(findmnt -no SOURCE --target "$TARGET_DIR" 2>/dev/null || true)"
echo "Backing source  : ${SRC_DEV:-unknown}"
findmnt -o TARGET,SOURCE,FSTYPE,SIZE,USED,AVAIL --target "$TARGET_DIR" 2>/dev/null || true
echo ""
lsblk -o NAME,SIZE,TYPE,TRAN,ROTA,MODEL,MOUNTPOINT 2>/dev/null || true
# rotational + transport for the specific backing device
BASE_DEV="$(lsblk -no PKNAME "$SRC_DEV" 2>/dev/null | head -1)"
[ -z "$BASE_DEV" ] && BASE_DEV="$(basename "${SRC_DEV:-}")"
if [ -n "$BASE_DEV" ] && [ -e "/sys/block/$BASE_DEV/queue/rotational" ]; then
  echo "Device $BASE_DEV rotational=$(cat /sys/block/$BASE_DEV/queue/rotational) (0=SSD/NVMe, 1=HDD)"
fi

# --- Ensure fio is available ---
section "Ensuring fio is installed"
if ! command -v fio >/dev/null 2>&1; then
  echo "fio not found; attempting install..."
  sudo apt-get update -y >/dev/null 2>&1 && sudo apt-get install -y fio >/dev/null 2>&1 || true
fi
if ! command -v fio >/dev/null 2>&1; then
  echo "ERROR: fio unavailable; falling back to dd sequential-only."
  TEST="$TARGET_DIR/dd_probe.dat"
  echo "-- dd sequential write (O_DIRECT) --"
  dd if=/dev/zero of="$TEST" bs=1M count=$((SIZE_GB*1024)) oflag=direct conv=fdatasync 2>&1 | tail -1
  echo "-- dd sequential read (O_DIRECT) --"
  dd if="$TEST" of=/dev/null bs=1M iflag=direct 2>&1 | tail -1
  rm -f "$TEST"
  exit 0
fi
fio --version

TEST="$TARGET_DIR/fio_probe.dat"
IOENGINE="libaio"

run_scenario() { # name rw bs iodepth numjobs [rwmixread]
  local name="$1" rw="$2" bs="$3" iod="$4" nj="$5" mix="${6:-}"
  section "Scenario: $name"
  local extra=""
  [ -n "$mix" ] && extra="--rwmixread=$mix"
  local json
  json="$(fio --name="$name" --filename="$TEST" --direct=1 --ioengine="$IOENGINE" \
      --size="${SIZE_GB}G" --runtime="$DURATION" --time_based --rw="$rw" --bs="$bs" \
      --iodepth="$iod" --numjobs="$nj" --group_reporting --output-format=json $extra 2>/tmp/fio.err)"
  if [ -z "$json" ]; then echo "fio failed:"; cat /tmp/fio.err; return; fi
  echo "$json" | python3 -c "
import sys,json
d=json.load(sys.stdin); j=d['jobs'][0]
r=j['read']; w=j['write']
rb=r['bw_bytes']/1e6; wb=w['bw_bytes']/1e6
print(f'  read : {rb:8.2f} MB/s  {r[\"iops\"]:10.1f} IOPS')
print(f'  write: {wb:8.2f} MB/s  {w[\"iops\"]:10.1f} IOPS')
tot=rb+wb; tiops=r['iops']+w['iops']
print(f'RESULT|$LABEL|$name|{tot:.2f}|{tiops:.2f}')
"
}

# block size / pattern / queue depth chosen to mirror build-like I/O
run_scenario 'seq-write-1M'    write     1M 8 1
run_scenario 'seq-read-1M'     read      1M 8 1
run_scenario 'rand-read-4K'    randread  4k 8 4
run_scenario 'rand-write-4K'   randwrite 4k 8 4
run_scenario 'rand-rw-64K-30w' randrw    64k 8 4 70

rm -f "$TEST"
section "SUMMARY [$LABEL]"
echo "See RESULT| lines above (aggregate MB/s and IOPS per scenario)."

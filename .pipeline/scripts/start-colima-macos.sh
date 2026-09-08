#!/bin/bash
# Boots a Colima VM on a Microsoft-hosted macOS agent from a prebuilt toolchain.
#
# Nothing is installed here and nothing is downloaded: TOOLCHAIN_DIR points at a
# payload of docker, colima, lima and colima's guest image, fetched from our own
# feed by .pipeline/templates/macos-docker-steps.yml and produced by
# .pipeline/macos-docker-toolchain-pipeline.yml. A run therefore contacts
# neither Homebrew nor ghcr.io nor github.com, which is the point: Homebrew
# dropping the Intel docker bottle at 29.8.0 turned `brew install docker` into
# an 8-minute Go build, and colima's image download has failed on DNS.
#
# Colima's VM boot is flaky on hosted macOS (~3% of runs): the lima hostagent
# either misses its 5s startup window or never emits the `running` event. Both
# are transient, so retry from a clean slate instead of failing the job.
set -euo pipefail

# shellcheck source=.pipeline/scripts/run-bounded.sh
. "$(dirname "$0")/run-bounded.sh"

COLIMA_CPU=${COLIMA_CPU:-4}
COLIMA_DISK=${COLIMA_DISK:-50}
# Stays at the long-standing 4GiB. The startup crashes the SQL Server script
# retries are not memory-related (every captured one reports oom=false and dies
# at LSA load, before the buffer pool is committed), and a larger VM measurably
# slows boot.
COLIMA_MEMORY=${COLIMA_MEMORY:-4}
COLIMA_START_ATTEMPTS=${COLIMA_START_ATTEMPTS:-3}
# Catches a genuinely wedged `colima start` so it still reaches the delete and
# retry path instead of running until the pipeline step timeout. Sized above the
# slowest healthy boot seen over 113 runs (509s; p95 366s) — the observed
# failures give up within seconds, so anything past this is stuck, not slow.
COLIMA_START_TIMEOUT_SECONDS=${COLIMA_START_TIMEOUT_SECONDS:-540}
# The macOS job only gets 60 minutes, so cap the retries by wall clock rather
# than letting three boots of an unhealthy agent eat the test budget.
COLIMA_BUDGET_SECONDS=${COLIMA_BUDGET_SECONDS:-480}
# The payload to run from. Required: there is no second way to get a docker CLI
# onto the agent, and a run that quietly found one some other way would not be
# the run we tested.
TOOLCHAIN_DIR=${TOOLCHAIN_DIR:-}

fail() {
  echo "##[error]$1"
  exit 1
}

manifest_field() {
  python3 -c 'import json, sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value = value[key]
print(value)' "$TOOLCHAIN_DIR/manifest.json" "$@"
}

# Manifest values that become paths are checked rather than trusted. The
# payload is ours, but "it came from our feed" is not the same as "it is
# well-formed", and these are joined onto a directory.
plain_name() {
  case "$1" in
    "" | . | .. | */* | *[!A-Za-z0-9._-]*)
      fail "toolchain manifest gave $2 as '$1', which is not a plain file name" ;;
  esac
}

# Puts the packaged toolchain on PATH and seeds colima's image cache. Nothing
# here is bounded, unlike `colima start` below: it is local file work against a
# payload the agent already has.
install_from_package() {
  [ -n "$TOOLCHAIN_DIR" ] \
    || fail "TOOLCHAIN_DIR is not set; this script runs from the packaged toolchain (see .pipeline/templates/macos-docker-steps.yml)"
  [ -f "$TOOLCHAIN_DIR/manifest.json" ] \
    || fail "no manifest.json in the toolchain payload at $TOOLCHAIN_DIR"

  # Universal Packages do not carry POSIX modes, so the executable bit does not
  # survive the round trip through the feed.
  chmod -R +x "$TOOLCHAIN_DIR/bin" "$TOOLCHAIN_DIR/libexec" 2>/dev/null || true
  export PATH="$TOOLCHAIN_DIR/bin:$PATH"
  echo "##vso[task.prependpath]$TOOLCHAIN_DIR/bin"

  # Checked here rather than left to fail at the first use: `colima start`
  # reporting a missing limactl is a much longer walk back to "the payload was
  # incomplete".
  local tool
  for tool in docker colima limactl; do
    [ -x "$TOOLCHAIN_DIR/bin/$tool" ] || fail "toolchain payload has no executable bin/$tool"
  done

  # colima looks the guest image up in this cache by sha256 of the URL it would
  # otherwise download it from, so seeding it under that name is what keeps the
  # ~350 MB fetch from github.com out of the job.
  local cache_dir="$HOME/Library/Caches/colima/caches"
  local cache_name image_name cached image
  cache_name=$(manifest_field image cache_filename)
  image_name=$(manifest_field image filename)
  plain_name "$cache_name" "image.cache_filename"
  plain_name "$image_name" "image.filename"
  cached="$cache_dir/$cache_name"
  image="$TOOLCHAIN_DIR/image/$image_name"
  [ -f "$image" ] || fail "toolchain payload has no guest image at $image"
  mkdir -p "$cache_dir"
  [ -f "$cached" ] || cp "$image" "$cached"

  echo "toolchain: $(manifest_field arch) payload, layout $(manifest_field payload_format), id $(manifest_field identity)"
  echo "guest image seeded at $cached ($(du -h "$cached" | cut -f1))"
}

install_from_package

docker --version
colima version | head -1

start_time=$(date +%s)
deadline=$((start_time + COLIMA_BUDGET_SECONDS))
attempts=0

while [ "$attempts" -lt "$COLIMA_START_ATTEMPTS" ]; do
  limit=$COLIMA_START_TIMEOUT_SECONDS
  # The first attempt always gets a full slot; later ones take what is left.
  if [ "$attempts" -gt 0 ]; then
    left=$((deadline - $(date +%s)))
    if [ "$left" -le 30 ]; then
      echo "##[warning]colima retry budget (${COLIMA_BUDGET_SECONDS}s) exhausted after $attempts attempt(s)"
      break
    fi
    [ "$left" -lt "$limit" ] && limit=$left
  fi

  attempts=$((attempts + 1))
  echo "##[group]colima start (attempt $attempts/$COLIMA_START_ATTEMPTS, ${limit}s limit)"
  # The only bounded command in this script, and it must stay that way: on
  # timeout run_bounded signals the bounded command's own process group, so a
  # wedged boot cannot leave limactl or qemu alive holding the task's stdout.
  # Nesting another run_bounded around this one would put that group out of
  # reach of the outer kill and reintroduce exactly that orphan.
  if run_bounded "$limit" colima start --cpu "$COLIMA_CPU" --memory "$COLIMA_MEMORY" --disk "$COLIMA_DISK"; then
    echo "##[endgroup]"
    docker context use colima >/dev/null || true
    docker version
    docker ps
    exit 0
  fi
  echo "##[endgroup]"
  echo "##[warning]colima failed to start (attempt $attempts/$COLIMA_START_ATTEMPTS)"
  tail -50 "$HOME/.colima/_lima/colima/ha.stderr.log" 2>/dev/null || true
  colima delete --force >/dev/null 2>&1 || true
  sleep 5
done

echo "##[error]colima did not start: $attempts of $COLIMA_START_ATTEMPTS configured attempt(s) ran in $(($(date +%s) - start_time))s of a ${COLIMA_BUDGET_SECONDS}s budget"
exit 1

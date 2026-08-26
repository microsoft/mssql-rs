#!/bin/bash
# Installs Docker + Colima on a Microsoft-hosted macOS agent and boots the VM.
#
# Colima's VM boot is flaky on hosted macOS (~3% of runs): the lima hostagent
# either misses its 5s startup window or never emits the `running` event. Both
# are transient, so retry from a clean slate instead of failing the job.
set -euo pipefail

COLIMA_CPU=${COLIMA_CPU:-4}
COLIMA_DISK=${COLIMA_DISK:-50}
# Stays at the long-standing 4GiB. The startup crashes this script retries are
# not memory-related (every captured one reports oom=false and dies at LSA load,
# before the buffer pool is committed), and a larger VM measurably slows boot.
COLIMA_MEMORY=${COLIMA_MEMORY:-4}
COLIMA_START_ATTEMPTS=${COLIMA_START_ATTEMPTS:-3}
# The macOS job only gets 60 minutes, so cap the retries by wall clock rather
# than letting three boots of an unhealthy agent eat the test budget.
COLIMA_BUDGET_SECONDS=${COLIMA_BUDGET_SECONDS:-480}

brew update
brew install docker colima

start_time=$(date +%s)
for attempt in $(seq 1 "$COLIMA_START_ATTEMPTS"); do
  if [ "$attempt" -gt 1 ] && [ $(($(date +%s) - start_time)) -ge "$COLIMA_BUDGET_SECONDS" ]; then
    echo "##[warning]colima retry budget (${COLIMA_BUDGET_SECONDS}s) exhausted"
    break
  fi
  echo "##[group]colima start (attempt $attempt/$COLIMA_START_ATTEMPTS)"
  if colima start --cpu "$COLIMA_CPU" --memory "$COLIMA_MEMORY" --disk "$COLIMA_DISK"; then
    echo "##[endgroup]"
    docker context use colima >/dev/null || true
    docker version
    docker ps
    exit 0
  fi
  echo "##[endgroup]"
  echo "##[warning]colima failed to start (attempt $attempt/$COLIMA_START_ATTEMPTS)"
  tail -50 "$HOME/.colima/_lima/colima/ha.stderr.log" 2>/dev/null || true
  colima delete --force >/dev/null 2>&1 || true
  sleep 5
done

echo "##[error]colima did not start after $COLIMA_START_ATTEMPTS attempts"
exit 1

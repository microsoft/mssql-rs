#!/bin/bash
# Installs Docker + Colima on a Microsoft-hosted macOS agent and boots the VM.
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

brew update
brew install docker colima

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

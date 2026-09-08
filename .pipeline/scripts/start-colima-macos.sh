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
# `brew install docker` is deliberately not used bare. Homebrew ships no bottle
# for the docker CLI on Intel macOS as of 29.8.0, so a bare install compiles it
# from source and builds Go (~8 min) to do so. Measured over 147 runs: agents
# that resolved the bottled 29.7.2 finished this phase in 29s median and failed
# 1% of the time, while agents that built 29.8.0 took 441s median (774s max) and
# failed 36% — the build alone exhausted the step timeout.
#
# `--force-bottle` makes brew fail rather than fall back to source. When it does
# fail, install-brew-bottle.py takes the newest version that *is* bottled for
# this platform straight from Homebrew's registry, so there is no version to pin
# by hand and no third-party download. Both paths therefore install exactly what
# Homebrew would have, and this self-heals once the current version is bottled
# again — which on arm64 it already is.
DOCKER_CLI_DIR=${DOCKER_CLI_DIR:-$HOME/.docker-cli/bin}
# Bounded so a slow install fails here with a message rather than silently
# consuming the step budget and surfacing as an opaque "task has timed out".
INSTALL_TIMEOUT_SECONDS=${INSTALL_TIMEOUT_SECONDS:-300}

install_tooling() {
  # `set -e` is suppressed inside a function called from a conditional, so every
  # prerequisite has to report failure explicitly or a broken brew would look
  # like a successful install and only surface as a confusing colima failure.
  brew update || return 1
  brew install colima || return 1

  if brew install --force-bottle docker; then
    return 0
  fi
  echo "##[warning]No docker CLI bottle for the current version on this platform; falling back to the newest bottled version"
  python3 "$(dirname "$0")/install-brew-bottle.py" docker "$DOCKER_CLI_DIR" || return 1
}

# A leftover directory would make the "did we fall back?" check below lie.
rm -rf "$DOCKER_CLI_DIR"

install_status=0
run_bounded "$INSTALL_TIMEOUT_SECONDS" install_tooling || install_status=$?
if [ "$install_status" -eq 124 ]; then
  echo "##[error]Installing colima and the docker CLI did not finish within ${INSTALL_TIMEOUT_SECONDS}s"
  exit 1
elif [ "$install_status" -ne 0 ]; then
  echo "##[error]Installing colima and the docker CLI failed (exit $install_status)"
  exit 1
fi

# Only the fallback populates DOCKER_CLI_DIR; brew's own docker is already on PATH.
# prependpath only affects later steps, so also fix PATH for this one.
if [ -x "$DOCKER_CLI_DIR/docker" ]; then
  export PATH="$DOCKER_CLI_DIR:$PATH"
  echo "##vso[task.prependpath]$DOCKER_CLI_DIR"
fi
docker --version

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

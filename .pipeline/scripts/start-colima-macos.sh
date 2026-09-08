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
# `--force-bottle` makes brew refuse to build the *requested* formula from
# source, but Homebrew dropping bottle support for Intel macOS entirely
# (September 2026) exposed a gap that assumption doesn't cover: brew still
# resolves and starts fetching source for docker's *build dependencies* (go,
# go-md2man) before it gets around to reporting "docker has no bottle", so the
# attempt can hang well past that error on a platform with no bottles at all.
# Bounded separately and tightly so that hang is cut short well before it could
# exhaust the whole install budget, falling through instead to the fallback
# below, which doesn't invoke brew and isn't subject to this at all. No
# measured p95/max for this one, unlike the other limits in this file: a
# healthy-but-slow bottled install that this cuts off still lands on the
# fallback and still installs docker, just slower, so the failure mode of
# sizing this wrong is "occasionally takes the slower path", not a stuck step.
#
# When it does fail (or is cut short), install-brew-bottle.py takes the newest
# version that *is* bottled for this platform straight from Homebrew's
# registry, so there is no version to pin by hand and no third-party download.
# Both paths therefore install exactly what Homebrew would have, and this
# self-heals once the current version is bottled again — which on arm64 it
# already is.
DOCKER_CLI_DIR=${DOCKER_CLI_DIR:-$HOME/.docker-cli/bin}
DOCKER_BOTTLE_TIMEOUT_SECONDS=${DOCKER_BOTTLE_TIMEOUT_SECONDS:-90}
# Bounded so a slow install fails here with a message rather than silently
# consuming the step budget and surfacing as an opaque "task has timed out".
INSTALL_TIMEOUT_SECONDS=${INSTALL_TIMEOUT_SECONDS:-300}
# A payload from our own feed, published by
# .pipeline/macos-docker-toolchain-pipeline.yml. When set, none of the Homebrew
# reasoning above applies: the toolchain is already resolved, verified and
# pinned, so the job reaches nothing but Azure Artifacts. The brew path below
# stays for runs that do not have one, such as a developer running this script.
TOOLCHAIN_DIR=${TOOLCHAIN_DIR:-}

# A leftover directory would make the "did we fall back?" check below lie.
rm -rf "$DOCKER_CLI_DIR"

# Every install step is bounded directly by run_bounded here, one call each,
# none nested inside another. run_bounded's timeout path signals the bounded
# command's *own* process group (`set -m` gives it one), specifically so a
# hang doesn't leave a grandchild alive holding the task's stdout after the
# bound fires. Wrapping this whole sequence in one more, outer run_bounded --
# as an earlier version of this script did, backgrounding the function that
# contains these calls -- would put that function in its own group and each
# command it bounds in a *further* nested group of its own: if the outer
# bound fired while a command was still inside its own inner bound, the outer
# kill would reach the function's group but not the nested command's, leaving
# exactly the orphan this helper exists to prevent. Flat, single-level bounds
# don't have that failure mode: whichever bound owns a command is the only
# one that can ever signal it.
install_deadline=$(( $(date +%s) + INSTALL_TIMEOUT_SECONDS ))

fail_install() {
  echo "##[error]$1"
  exit 1
}

# Whatever remains of the overall install budget, capped at $1 when given
# and smaller, or 0 once the budget is exhausted. Never calls fail_install
# itself: every caller reads this through a plain `$(...)` command
# substitution, where an `exit` only ends that subshell, not the script --
# so the 0 case has to be checked, and failed, by the caller instead.
budget_limit() {
  local cap=${1:-}
  local left=$((install_deadline - $(date +%s)))
  [ "$left" -gt 0 ] || left=0
  if [ -n "$cap" ] && [ "$cap" -lt "$left" ]; then
    echo "$cap"
  else
    echo "$left"
  fi
}

# Bounds "$@" by whatever remains of the overall install budget, failing the
# step immediately -- rather than letting a later step start against an
# already-exhausted or barely-alive budget -- on timeout or a non-zero exit.
run_install_step() {
  local limit
  limit=$(budget_limit)
  if [ "$limit" -eq 0 ]; then
    fail_install "Installing colima and the docker CLI did not finish within ${INSTALL_TIMEOUT_SECONDS}s"
  fi
  # Not `run_bounded ... ; local status=$?`: under `set -e`, a non-zero
  # exit from a plain (untested) command aborts the script on the spot,
  # before this function ever reaches the `local` line to capture it. `||`
  # is a test, so it's the only way to observe the real code here.
  local status=0
  run_bounded "$limit" "$@" || status=$?
  if [ "$status" -eq 124 ]; then
    fail_install "Installing colima and the docker CLI did not finish within ${INSTALL_TIMEOUT_SECONDS}s"
  elif [ "$status" -ne 0 ]; then
    fail_install "Installing colima and the docker CLI failed (exit $status)"
  fi
}

manifest_field() {
  python3 -c "import json,sys;print(json.load(open(sys.argv[1]))$1)" "$TOOLCHAIN_DIR/manifest.json"
}

# Puts the packaged toolchain on PATH and seeds colima's image cache, so
# neither Homebrew nor github.com is contacted. Nothing here is bounded: it is
# local file work against a payload the agent already has.
install_from_package() {
  [ -f "$TOOLCHAIN_DIR/manifest.json" ] \
    || fail_install "no manifest.json in the toolchain payload at $TOOLCHAIN_DIR"

  # Universal Packages do not carry POSIX modes, so the executable bit does not
  # survive the round trip through the feed.
  chmod -R +x "$TOOLCHAIN_DIR/bin" "$TOOLCHAIN_DIR/libexec" 2>/dev/null || true
  export PATH="$TOOLCHAIN_DIR/bin:$PATH"
  echo "##vso[task.prependpath]$TOOLCHAIN_DIR/bin"

  # colima looks the guest image up in this cache by sha256 of the URL it would
  # otherwise download it from, so seeding it under that name is what keeps the
  # ~350 MB fetch from github.com out of the job.
  local cache_dir="$HOME/Library/Caches/colima/caches"
  local cached image
  cached="$cache_dir/$(manifest_field "['image']['cache_filename']")"
  image="$TOOLCHAIN_DIR/image/$(manifest_field "['image']['filename']")"
  [ -f "$image" ] || fail_install "toolchain payload has no guest image at $image"
  mkdir -p "$cache_dir"
  [ -f "$cached" ] || cp "$image" "$cached"

  echo "toolchain: $(manifest_field "['arch']") payload, layout $(manifest_field "['payload_format']"), id $(manifest_field "['identity']")"
  echo "guest image seeded at $cached ($(du -h "$cached" | cut -f1))"
}

if [ -n "$TOOLCHAIN_DIR" ]; then
  install_from_package
else
  run_install_step brew update
  run_install_step brew install colima

  # The docker-bottle attempt is the one step allowed to fail without ending
  # the job: that failure is the expected, handled path into the fallback, not
  # an install error. It is also the only step that needs a cap tighter than
  # the overall budget (DOCKER_BOTTLE_TIMEOUT_SECONDS), which is why it can't
  # go through run_install_step: that helper treats any non-zero exit as
  # fatal.
  bottle_limit=$(budget_limit "$DOCKER_BOTTLE_TIMEOUT_SECONDS")
  if [ "$bottle_limit" -eq 0 ]; then
    fail_install "Installing colima and the docker CLI did not finish within ${INSTALL_TIMEOUT_SECONDS}s"
  fi
  if ! run_bounded "$bottle_limit" brew install --force-bottle docker; then
    echo "##[warning]No docker CLI bottle for the current version on this platform (or the attempt ran past ${bottle_limit}s); falling back to the newest bottled version"
    run_install_step python3 "$(dirname "$0")/install-brew-bottle.py" docker "$DOCKER_CLI_DIR"
  fi

  # Only the fallback populates DOCKER_CLI_DIR; brew's own docker is already on PATH.
  # prependpath only affects later steps, so also fix PATH for this one.
  if [ -x "$DOCKER_CLI_DIR/docker" ]; then
    export PATH="$DOCKER_CLI_DIR:$PATH"
    echo "##vso[task.prependpath]$DOCKER_CLI_DIR"
  fi
fi

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

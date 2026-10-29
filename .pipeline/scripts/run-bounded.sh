#!/bin/bash
# Sourced helper. macOS ships no coreutils `timeout`, so bound a long-running
# command by hand: run it in the background and kill it if it overruns.
#
# Returns 124 when the limit was hit, otherwise the command's own exit status.

run_bounded() {
  local limit=$1 pid waited=0
  shift
  # Job control gives the child its own process group. Signalling only the direct
  # child leaves grandchildren (limactl, qemu, docker) alive holding the task's
  # stdout, which hangs the pipeline step even after the timeout fires.
  set -m
  "$@" &
  pid=$!
  set +m
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge "$limit" ]; then
      echo "##[warning]'$1' exceeded ${limit}s — terminating"
      kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
      sleep 5
      kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      return 124
    fi
    sleep 2
    waited=$((waited + 2))
  done
  wait "$pid"
}

#!/bin/bash
# Sourced helper. macOS ships no coreutils `timeout`, so bound a long-running
# command by hand: run it in the background and kill it if it overruns.
#
# Returns 124 when the limit was hit, otherwise the command's own exit status.

run_bounded() {
  local limit=$1 pid='' waited=0 result='' previous_term previous_job=${!:-} cleanup_attempt
  shift
  previous_term=$(trap -p TERM)
  # A parent may cancel this shell while the bounded command is in a separate
  # process group. Stop that group before exiting instead of orphaning it.
  trap '
    if [ -z "$pid" ] && [ "${!:-}" != "$previous_job" ]; then
      pid=${!:-}
    fi
    if [ -n "$pid" ]; then
      kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
      if ! kill -KILL -- "-$pid" 2>/dev/null &&
         ! kill -KILL "$pid" 2>/dev/null &&
         kill -0 "$pid" 2>/dev/null; then
        echo "##[error]Could not terminate bounded process group $pid" >&2
        exit 143
      fi
      wait "$pid" 2>/dev/null || true
      for cleanup_attempt in 1 2 3 4 5; do
        if ! kill -0 -- "-$pid" 2>/dev/null; then
          break
        fi
        sleep 0.1
      done
      if kill -0 -- "-$pid" 2>/dev/null; then
        echo "##[error]Bounded process group $pid did not terminate" >&2
      fi
    fi
    exit 143
  ' TERM
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
      result=124
      break
    fi
    sleep 2
    waited=$((waited + 2))
  done
  if [ -z "$result" ]; then
    if wait "$pid"; then result=0; else result=$?; fi
  fi
  if [ -n "$previous_term" ]; then
    eval "$previous_term"
  else
    trap - TERM
  fi
  return "$result"
}

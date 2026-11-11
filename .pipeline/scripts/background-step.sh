#!/bin/bash
# Runs a command detached from the current pipeline step and joins it in a
# later one, so the steps in between can overlap with it.
#
#   background-step.sh start <state-dir> <command> [args...]
#   background-step.sh wait  <state-dir> <timeout-seconds>
#
# `wait` replays the command's output and exits with its status. Replaying in
# the wait step is what makes the command's ##[error] lines and ##vso commands
# (for example task.prependpath) take effect, since the agent only parses the
# output of the step that is running. The timeout counts from `start`, so it
# bounds the command's total wall clock rather than only the wait.
set -euo pipefail

usage() {
  echo "usage: $0 start <state-dir> <command> [args...]" >&2
  echo "       $0 wait <state-dir> <timeout-seconds>" >&2
  exit 2
}

[ $# -ge 2 ] || usage
action=$1
state=$2
shift 2

start() {
  [ $# -ge 1 ] || usage
  rm -rf "$state"
  mkdir -p "$state"
  date +%s >"$state/started"
  # Every stream is redirected: the agent keeps a step open until the handles
  # it gave the step are closed. Job control isolates the command's process
  # group so a later timeout can stop its descendants too.
  set -m
  nohup bash -c '
    "${@:2}"
    printf "%s %s\n" "$?" "$(date +%s)" >"$1/status.tmp"
    mv "$1/status.tmp" "$1/status"
  ' _ "$state" "$@" </dev/null >"$state/log" 2>&1 &
  local pid=$!
  set +m
  echo "$pid" >"$state/pid"
  echo "Started in the background (pid $pid): $*"
}

terminate_group() {
  local pid=$1 attempt
  if ! kill -0 -- "-$pid" 2>/dev/null; then
    return 0
  fi
  if ! kill -TERM -- "-$pid" 2>/dev/null && kill -0 -- "-$pid" 2>/dev/null; then
    echo "##[warning]Could not send SIGTERM to process group $pid"
  fi
  for ((attempt = 0; attempt < 20; attempt++)); do
    if ! kill -0 -- "-$pid" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  if ! kill -KILL -- "-$pid" 2>/dev/null && kill -0 -- "-$pid" 2>/dev/null; then
    return 1
  fi
  for ((attempt = 0; attempt < 20; attempt++)); do
    if ! kill -0 -- "-$pid" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  ! kill -0 -- "-$pid" 2>/dev/null
}

fail_timeout() {
  local timeout=$1
  cat "$state/log"
  echo "##[error]The background command did not finish within ${timeout}s of starting"
  exit 1
}

join() {
  [ $# -eq 1 ] || usage
  [[ $1 =~ ^[0-9]+$ ]] || usage
  local timeout=$1 started pid deadline waiting_since code finished
  if [ ! -f "$state/started" ] || [ ! -f "$state/pid" ]; then
    echo "##[error]No background command was started in $state"
    exit 1
  fi
  started=$(cat "$state/started")
  pid=$(cat "$state/pid")
  deadline=$((started + timeout))
  waiting_since=$(date +%s)
  while [ ! -f "$state/status" ]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      # The wrapper writes the status just before it exits.
      sleep 1
      [ -f "$state/status" ] && break
      cat "$state/log"
      echo "##[error]The background command exited without recording a status"
      exit 1
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      [ -f "$state/status" ] && break
      if ! terminate_group "$pid"; then
        echo "##[error]Could not stop background process group $pid"
      fi
      fail_timeout "$timeout"
    fi
    sleep 2
  done
  read -r code finished <"$state/status"
  if [ "$finished" -ge "$deadline" ]; then
    fail_timeout "$timeout"
  fi
  cat "$state/log"
  echo "The background command ran for $((finished - started))s; this step waited $(($(date +%s) - waiting_since))s."
  exit "$code"
}

case "$action" in
  start) start "$@" ;;
  wait) join "$@" ;;
  *) usage ;;
esac

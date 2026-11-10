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
  # it gave the step are closed.
  nohup bash -c '
    "${@:2}"
    printf "%s %s\n" "$?" "$(date +%s)" >"$1/status.tmp"
    mv "$1/status.tmp" "$1/status"
  ' _ "$state" "$@" </dev/null >"$state/log" 2>&1 &
  echo "$!" >"$state/pid"
  echo "Started in the background (pid $!): $*"
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
      cat "$state/log"
      echo "##[error]The background command did not finish within ${timeout}s of starting"
      exit 1
    fi
    sleep 2
  done
  read -r code finished <"$state/status"
  cat "$state/log"
  echo "The background command ran for $((finished - started))s; this step waited $(($(date +%s) - waiting_since))s."
  exit "$code"
}

case "$action" in
  start) start "$@" ;;
  wait) join "$@" ;;
  *) usage ;;
esac

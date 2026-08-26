#!/bin/bash
# Starts the SQL Server test container inside the Colima VM on macOS agents.
#
# Two transient failures dominate this step on hosted macOS agents:
#
#  1. `docker pull` occasionally fails with "failed to resolve reference" — a
#     registry/network blip, so the pull is retried.
#  2. SQL Server itself crashes during startup in roughly 1 in 8 runs. The
#     container exits with code 1 and the log shows a SQLPAL fatal error
#     ("[AppLoader] Failed to load LSA: 0xc0070102" or reason 0x2) rather than
#     an OOM kill. It is a startup race in the emulated VM, and simply
#     recreating the container clears it.
#
# The readiness probe therefore watches container liveness as well as
# connectivity: once the container has exited there is no point probing for the
# remaining time, so the whole container is recreated instead. Failing that,
# the script exits non-zero — the previous inline loop fell through silently,
# leaving pytest to grind against a dead server until the 60-minute job timeout.
#
# Retries are bounded by wall-clock deadlines rather than attempt counts,
# because the macOS job only gets 60 minutes in total. A failed sqlcmd probe
# costs ~11s by default and `docker pull` of the SQL image runs p50 292s / max
# 724s here, so counting attempts hides very large amounts of time. Every bound
# is sized above the worst case of the *successful* runs so it only catches a
# hang; retrying stops once there is not enough budget left for another attempt.
set -euo pipefail

# shellcheck source=.pipeline/scripts/run-bounded.sh
. "$(dirname "$0")/run-bounded.sh"

: "${SQL_PASSWORD:?SQL_PASSWORD must be set}"

SQL_IMAGE=${SQL_IMAGE:-mcr.microsoft.com/mssql/server:2025-latest}
SQL_CONTAINER=${SQL_CONTAINER:-sqlserver}

# Total wall-clock budget for the step (pull plus every container attempt).
SETUP_BUDGET_SECONDS=${SETUP_BUDGET_SECONDS:-1200}
# Sub-budget for the pull phase, so a slow registry cannot starve the retries.
# Sized above the slowest pull seen in 112 runs (724s; p95 478s) — pulls are slow
# here, not flaky, so a tighter bound just kills healthy runs.
PULL_BUDGET_SECONDS=${PULL_BUDGET_SECONDS:-900}
# Per-container-attempt allowance. Slowest healthy startup over 97 runs was 157s.
READY_TIMEOUT_SECONDS=${READY_TIMEOUT_SECONDS:-240}
# A container that exits immediately is abandoned in seconds, so cap the
# attempts too — otherwise the budget alone would allow dozens of restarts.
MAX_START_ATTEMPTS=${MAX_START_ATTEMPTS:-3}
# Login timeout per probe; the sqlcmd default of ~9s makes probes expensive.
PROBE_LOGIN_TIMEOUT_SECONDS=${PROBE_LOGIN_TIMEOUT_SECONDS:-5}
PROBE_INTERVAL_SECONDS=${PROBE_INTERVAL_SECONDS:-3}

START_TIME=$(date +%s)
SETUP_DEADLINE=$((START_TIME + SETUP_BUDGET_SECONDS))

elapsed() { echo $(($(date +%s) - START_TIME)); }

pull_image() {
  local deadline=$((START_TIME + PULL_BUDGET_SECONDS)) attempt=0 left
  while :; do
    attempt=$((attempt + 1))
    left=$((deadline - $(date +%s)))
    if [ "$left" -le 30 ]; then
      echo "##[error]Unable to pull $SQL_IMAGE within ${PULL_BUDGET_SECONDS}s"
      return 1
    fi
    if run_bounded "$left" docker pull "$SQL_IMAGE"; then
      echo "Image pulled after $(elapsed)s."
      return 0
    fi
    echo "##[warning]docker pull failed (attempt $attempt)"
    sleep 10
  done
}

dump_diagnostics() {
  set +e
  echo "##[group]SQL Server container diagnostics"
  docker ps -a
  docker inspect "$SQL_CONTAINER" --format \
    'status={{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}} error={{.State.Error}}'
  docker logs --tail 200 "$SQL_CONTAINER"
  echo "##[endgroup]"
  set -e
}

# Returns 0 once the container answers SELECT 1; 1 if it died or stayed
# unreachable for the whole allowance.
wait_until_ready() {
  local limit=$1 deadline probe=0
  deadline=$(($(date +%s) + limit))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if [ "$(docker inspect -f '{{.State.Running}}' "$SQL_CONTAINER" 2>/dev/null)" != "true" ]; then
      echo "  container is no longer running — abandoning this attempt"
      return 1
    fi
    probe=$((probe + 1))
    if run_bounded 30 docker exec "$SQL_CONTAINER" /opt/mssql-tools18/bin/sqlcmd \
      -S localhost -U SA -P "$SQL_PASSWORD" \
      -C -b -l "$PROBE_LOGIN_TIMEOUT_SECONDS" -Q "SELECT 1" >/dev/null 2>&1; then
      echo "SQL Server is ready after $(elapsed)s (probe $probe)."
      return 0
    fi
    echo "  probe $probe, $((deadline - $(date +%s)))s left for this attempt..."
    sleep "$PROBE_INTERVAL_SECONDS"
  done
  echo "  SQL Server never answered within ${limit}s"
  return 1
}

pull_image

attempt=0
while :; do
  attempt=$((attempt + 1))
  left=$((SETUP_DEADLINE - $(date +%s)))
  if [ "$attempt" -gt "$MAX_START_ATTEMPTS" ]; then
    echo "##[error]SQL Server did not become ready after $MAX_START_ATTEMPTS attempts ($(elapsed)s used)"
    exit 1
  fi
  if [ "$left" -lt "$READY_TIMEOUT_SECONDS" ]; then
    echo "##[error]SQL Server not ready and only ${left}s of the ${SETUP_BUDGET_SECONDS}s budget remains after $((attempt - 1)) attempt(s)"
    exit 1
  fi

  echo "Starting $SQL_CONTAINER (attempt $attempt/$MAX_START_ATTEMPTS, ${left}s of budget left)..."
  docker rm --force "$SQL_CONTAINER" >/dev/null 2>&1 || true
  docker run \
    --name "$SQL_CONTAINER" \
    -e ACCEPT_EULA=Y \
    -e "MSSQL_SA_PASSWORD=$SQL_PASSWORD" \
    -p 1433:1433 \
    -d "$SQL_IMAGE"

  echo "Waiting for SQL Server to start..."
  if wait_until_ready "$READY_TIMEOUT_SECONDS"; then
    exit 0
  fi

  dump_diagnostics
  echo "##[warning]SQL Server failed readiness checks (attempt $attempt/$MAX_START_ATTEMPTS)"
done

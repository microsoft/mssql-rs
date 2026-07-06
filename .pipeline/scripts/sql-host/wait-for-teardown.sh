#!/usr/bin/env bash
# wait-for-teardown.sh — block until every expected teardown sentinel artifact
# has been published to the current build, then return so the caller can stop
# the SQL Server container.
#
# Required env:
#   EXPECTED_SENTINELS    Comma-separated list of pipeline-artifact names that
#                         dependent test jobs will publish in their always()
#                         step. The watcher returns once all of them exist.
#   BUILD_BUILDID         Current build id (System.BuildId).
#   SYSTEM_COLLECTIONURI  Org collection URI (e.g. https://dev.azure.com/foo/).
#   SYSTEM_TEAMPROJECT    Project name (System.TeamProject).
#   SYSTEM_ACCESSTOKEN    OAuth token; the calling step must pass it via env:.
#
# Optional env:
#   POLL_INTERVAL_SECONDS Default 15.
#   MAX_LIFETIME_MINUTES  Hard cap; default 120 (2h). Returns 0 with a warning
#                         once exceeded so the cleanup step can still tear down.
#                         Takes precedence over MAX_LIFETIME_SECONDS if both set.
#   MAX_LIFETIME_SECONDS  Same as above but in seconds (legacy).
#
# Exit codes:
#   0  All sentinels seen, OR max-lifetime hit (caller cleans up either way).
#   2  Misconfiguration (missing env, bad URL).

set -euo pipefail

: "${EXPECTED_SENTINELS:?EXPECTED_SENTINELS is required}"
: "${BUILD_BUILDID:?BUILD_BUILDID is required}"
: "${SYSTEM_COLLECTIONURI:?SYSTEM_COLLECTIONURI is required}"
: "${SYSTEM_TEAMPROJECT:?SYSTEM_TEAMPROJECT is required}"
: "${SYSTEM_ACCESSTOKEN:?SYSTEM_ACCESSTOKEN is required (expose via env: in YAML step)}"

POLL_INTERVAL_SECONDS="${POLL_INTERVAL_SECONDS:-15}"
if [ -n "${MAX_LIFETIME_MINUTES:-}" ]; then
    MAX_LIFETIME_SECONDS=$(( MAX_LIFETIME_MINUTES * 60 ))
fi
MAX_LIFETIME_SECONDS="${MAX_LIFETIME_SECONDS:-7200}"

# Normalise the collection URI to end with exactly one slash.
collection="${SYSTEM_COLLECTIONURI%/}/"
# URL-encode the project name minimally (handle spaces).
project_enc="$(printf '%s' "${SYSTEM_TEAMPROJECT}" | sed 's/ /%20/g')"
url="${collection}${project_enc}/_apis/build/builds/${BUILD_BUILDID}/artifacts?api-version=7.1"

IFS=',' read -r -a expected <<< "${EXPECTED_SENTINELS}"
echo "=== sql-host/wait-for-teardown.sh ==="
echo "  Build       = ${BUILD_BUILDID}"
echo "  Poll URL    = ${url}"
echo "  Poll every  = ${POLL_INTERVAL_SECONDS}s"
echo "  Max wait    = ${MAX_LIFETIME_SECONDS}s"
echo "  Expecting ${#expected[@]} sentinel(s):"
for s in "${expected[@]}"; do
    echo "    - ${s}"
done

deadline=$(( $(date +%s) + MAX_LIFETIME_SECONDS ))

while :; do
    now="$(date +%s)"
    if [ "${now}" -ge "${deadline}" ]; then
        echo "WARN: max lifetime (${MAX_LIFETIME_SECONDS}s) reached before all sentinels appeared. Proceeding to teardown." >&2
        exit 0
    fi

    # Fetch artifact list. -s = silent, -f = fail on HTTP >= 400.
    if ! body="$(curl -sS -f \
            -H "Authorization: Bearer ${SYSTEM_ACCESSTOKEN}" \
            -H "Accept: application/json" \
            "${url}" 2>/tmp/sql-host-curl.err)"; then
        echo "WARN: artifact list HTTP error: $(cat /tmp/sql-host-curl.err)" >&2
        sleep "${POLL_INTERVAL_SECONDS}"
        continue
    fi

    missing=0
    for s in "${expected[@]}"; do
        # Match exact JSON property: "name":"<sentinel>"
        if ! printf '%s' "${body}" | grep -qF "\"name\":\"${s}\""; then
            missing=$((missing + 1))
        fi
    done

    if [ "${missing}" -eq 0 ]; then
        echo "All ${#expected[@]} teardown sentinel(s) present. Releasing SQL host."
        exit 0
    fi

    remaining=$(( deadline - now ))
    echo "$(date -u +%H:%M:%S) waiting on ${missing}/${#expected[@]} sentinel(s); ${remaining}s left."
    sleep "${POLL_INTERVAL_SECONDS}"
done

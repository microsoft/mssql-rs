#!/usr/bin/env bash
# wait-for-teardown.sh — serve the SQL password to each consumer through a
# per-consumer key exchange, block until every expected teardown sentinel
# artifact has been published to the current build, then return so the caller
# can stop the SQL Server container.
#
# Key exchange: fork PR builds receive no secret variables, so the job-local
# sa password cannot reach consumers as a pipeline variable. Each consumer
# publishes an ephemeral RSA public key as `sql-key-<S>`, where <S> is its full
# teardown sentinel artifact name. For each key that appears, this script
# encrypts SQL_PASSWORD with RSA-OAEP (SHA-256, MGF1 SHA-256) and publishes the
# ciphertext as `sql-cred-<S>/credential.bin`. Only the holder of the private
# key, which never leaves the consumer step, can decrypt it.
#
# Required env:
#   EXPECTED_SENTINELS    Comma-separated list of pipeline-artifact names that
#                         dependent test jobs will publish in their always()
#                         step. The watcher returns once all of them exist.
#   SQL_PASSWORD          Job-local sa password to serve.
#   BUILD_BUILDID         Current build id (System.BuildId).
#   SYSTEM_COLLECTIONURI  Org collection URI (e.g. https://dev.azure.com/foo/).
#   SYSTEM_TEAMPROJECT    Project name (System.TeamProject).
#   SYSTEM_ACCESSTOKEN    OAuth token; the calling step must pass it via env:.
#   AGENT_TEMPDIRECTORY   Scratch space for key downloads and ciphertext.
#
# Optional env:
#   SENTINEL_SUFFIX       Appended to every expected sentinel name before
#                         matching (credential generation + stage attempt), so a
#                         stage rerun waits on that attempt's artifacts and
#                         never matches a previous attempt's leftovers.
#   POLL_INTERVAL_SECONDS Default 15.
#   MAX_LIFETIME_MINUTES  Hard cap; default 120 (2h). Returns 0 with a warning
#                         once exceeded so the cleanup step can still tear down.
#                         Takes precedence over MAX_LIFETIME_SECONDS if both set.
#   MAX_LIFETIME_SECONDS  Same as above but in seconds (legacy).
#
# Exit codes:
#   0  All sentinels seen, OR max-lifetime hit (caller cleans up either way).
#   2  Misconfiguration (missing env, bad URL, invalid password or sentinel).

set -euo pipefail

: "${EXPECTED_SENTINELS:?EXPECTED_SENTINELS is required}"
: "${BUILD_BUILDID:?BUILD_BUILDID is required}"
: "${SYSTEM_COLLECTIONURI:?SYSTEM_COLLECTIONURI is required}"
: "${SYSTEM_TEAMPROJECT:?SYSTEM_TEAMPROJECT is required}"
: "${SYSTEM_ACCESSTOKEN:?SYSTEM_ACCESSTOKEN is required (expose via env: in YAML step)}"
: "${AGENT_TEMPDIRECTORY:?AGENT_TEMPDIRECTORY is required}"

if [[ ! "${SQL_PASSWORD:-}" =~ ^Aa1![A-Za-z0-9]{22}$ ]]; then
    echo "ERROR: SQL_PASSWORD is missing or invalid." >&2
    exit 2
fi
if ! command -v openssl >/dev/null 2>&1; then
    echo "ERROR: openssl is required to serve SQL credentials." >&2
    exit 2
fi

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

# Namespacing: publishers suffix their teardown-sentinel artifact names with
# SENTINEL_SUFFIX (credential generation + stage attempt) so reruns neither collide with nor match
# a prior attempt's artifacts. Apply the same suffix to what we wait for.
SENTINEL_SUFFIX="${SENTINEL_SUFFIX:-}"
IFS=',' read -r -a expected_base <<< "${EXPECTED_SENTINELS}"
expected=()
served=()
for s in "${expected_base[@]}"; do
    # The name is embedded in ##vso logging commands and artifact paths.
    if [[ ! "${s}${SENTINEL_SUFFIX}" =~ ^[A-Za-z0-9_.-]+$ ]]; then
        echo "ERROR: invalid sentinel name '${s}${SENTINEL_SUFFIX}'." >&2
        exit 2
    fi
    expected+=( "${s}${SENTINEL_SUFFIX}" )
    served+=( "" )
done

# Prints the downloadUrl of artifact $1 from the artifact-list JSON on stdin.
artifact_download_url() {
    ARTIFACT="$1" python3 -c '
import json, os, sys
target = os.environ["ARTIFACT"]
for a in json.load(sys.stdin).get("value", []):
    if a.get("name") == target:
        print(a.get("resource", {}).get("downloadUrl", ""))
        break
'
}

# Encrypts SQL_PASSWORD to the consumer key at download URL $2 and publishes
# it as sql-cred-$1. Returns 0 once published, 1 to retry on the next poll,
# and 2 when the key is unusable (the consumer cannot republish the same name).
serve_credential() {
    local sentinel="$1" key_url="$2"
    local key_artifact="sql-key-${sentinel}" cred_artifact="sql-cred-${sentinel}"
    local work="${AGENT_TEMPDIRECTORY}/${key_artifact}"
    local out="${AGENT_TEMPDIRECTORY}/${cred_artifact}"
    local key_file bits

    rm -rf "${work}" "${out}"
    mkdir -p "${work}/extracted" "${out}" || return 1
    if ! curl -sS -f -L \
            -H "Authorization: Bearer ${SYSTEM_ACCESSTOKEN}" \
            -o "${work}/artifact.zip" \
            "${key_url}" 2>"${work}/curl.err"; then
        echo "WARN: ${key_artifact} download failed; retrying: $(cat "${work}/curl.err")" >&2
        return 1
    fi
    if ! python3 -m zipfile -e "${work}/artifact.zip" "${work}/extracted" 2>/dev/null; then
        echo "##vso[task.logissue type=warning]${key_artifact} is not a valid artifact archive; not serving it."
        return 2
    fi
    key_file="$(find "${work}/extracted" -type f -name key.pem | head -n 1)"
    bits=""
    if [ -n "${key_file}" ]; then
        bits="$(openssl rsa -pubin -in "${key_file}" -noout -text 2>/dev/null \
            | sed -n 's/.*(\([0-9][0-9]*\) bit).*/\1/p' | head -n 1)"
    fi
    if [ -z "${bits}" ] || [ "${bits}" -lt 3072 ]; then
        echo "##vso[task.logissue type=warning]${key_artifact} does not contain an RSA public key of at least 3072 bits; not serving it."
        return 2
    fi
    if ! printf '%s' "${SQL_PASSWORD}" | openssl pkeyutl -encrypt -pubin -inkey "${key_file}" \
            -pkeyopt rsa_padding_mode:oaep -pkeyopt rsa_oaep_md:sha256 -pkeyopt rsa_mgf1_md:sha256 \
            -out "${out}/credential.bin" 2>/dev/null; then
        echo "##vso[task.logissue type=warning]Encrypting the SQL credential to ${key_artifact} failed; not serving it."
        return 2
    fi
    # The agent uploads asynchronously and links the artifact while this step
    # keeps polling; it waits for pending uploads before the step completes.
    echo "##vso[artifact.upload containerfolder=${cred_artifact};artifactname=${cred_artifact}]${out}"
    echo "Served SQL credential to ${sentinel}."
}

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

    for i in "${!expected[@]}"; do
        s="${expected[$i]}"
        if [ -z "${served[$i]}" ] && [[ "${body}" == *"\"name\":\"sql-key-${s}\""* ]]; then
            key_url="$(printf '%s' "${body}" | artifact_download_url "sql-key-${s}" || true)"
            if [ -n "${key_url}" ]; then
                rc=0
                serve_credential "${s}" "${key_url}" || rc=$?
                case "${rc}" in
                    0) served[$i]=served ;;
                    2) served[$i]=rejected ;;
                esac
            fi
        fi
    done

    missing=0
    for s in "${expected[@]}"; do
        # Match exact JSON property: "name":"<sentinel>". A pattern match, not
        # `printf | grep -q`: under pipefail, grep exiting early on a large
        # artifact list can SIGPIPE printf and turn a hit into a miss.
        if [[ "${body}" != *"\"name\":\"${s}\""* ]]; then
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

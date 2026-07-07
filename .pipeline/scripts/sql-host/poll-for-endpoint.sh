#!/usr/bin/env bash
# poll-for-endpoint.sh — block until the matching SQL host has published its
# "sql-ready" sentinel artifact, then download it and export the SQL endpoint
# (host + port) as pipeline variables for downstream test steps.
#
# This is the symmetric counterpart of wait-for-teardown.sh on the SQL host
# side. Both jobs run concurrently (no ADO `dependsOn` between them, which
# would deadlock — ADO won't start a dependent job until its upstream
# completes, and the SQL host can only complete after tests finish). The two
# jobs rendezvous via pipeline-artifact sentinels in both directions:
#   * SQL host -> Test:  sql-ready-<instanceId>-<stageAttempt>  (endpoint)
#   * Test    -> SQL host: <sentinelName>-<stageAttempt>        (one per entry)
# The -<stageAttempt> (System.StageAttempt) suffix namespaces the rendezvous
# per stage attempt so a rerun forms a fresh handshake instead of colliding
# with or reading a prior attempt's artifacts. Set ARTIFACT_NAME accordingly.
#
# Required env:
#   ARTIFACT_NAME         Name of the sql-ready sentinel artifact.
#   BUILD_BUILDID
#   SYSTEM_COLLECTIONURI
#   SYSTEM_TEAMPROJECT
#   SYSTEM_ACCESSTOKEN    Expose via env: in the YAML step.
#   AGENT_TEMPDIRECTORY
#
# Optional env:
#   POLL_INTERVAL_SECONDS  default 10
#   MAX_WAIT_SECONDS       default 1800 (30 min). Covers x64 agent queueing,
#                          docker pull, and SQL Server startup.
#
# Sets pipeline variables on success:
#   DB_HOST   IP advertised by the SQL host
#   DB_PORT   port advertised by the SQL host

set -euo pipefail

: "${ARTIFACT_NAME:?ARTIFACT_NAME is required}"
: "${BUILD_BUILDID:?BUILD_BUILDID is required}"
: "${SYSTEM_COLLECTIONURI:?SYSTEM_COLLECTIONURI is required}"
: "${SYSTEM_TEAMPROJECT:?SYSTEM_TEAMPROJECT is required}"
: "${SYSTEM_ACCESSTOKEN:?SYSTEM_ACCESSTOKEN is required (expose via env: in YAML step)}"
: "${AGENT_TEMPDIRECTORY:?AGENT_TEMPDIRECTORY is required}"

POLL_INTERVAL_SECONDS="${POLL_INTERVAL_SECONDS:-10}"
# Generous default — the x64 SQL host job has its own queue + boot latency
# (DockerInstaller, image pull, SQL Server startup). 60 min comfortably
# covers a saturated pool while still being well under the SQL host's
# 120-min MAX_LIFETIME_MINUTES so we always fail-fast instead of racing
# the host's own teardown.
MAX_WAIT_SECONDS="${MAX_WAIT_SECONDS:-3600}"

collection="${SYSTEM_COLLECTIONURI%/}/"
project_enc="$(printf '%s' "${SYSTEM_TEAMPROJECT}" | sed 's/ /%20/g')"
url="${collection}${project_enc}/_apis/build/builds/${BUILD_BUILDID}/artifacts?api-version=7.1"

echo "=== sql-host/poll-for-endpoint.sh ==="
echo "  ARTIFACT_NAME = ${ARTIFACT_NAME}"
echo "  Build         = ${BUILD_BUILDID}"
echo "  Poll URL      = ${url}"
echo "  Poll every    = ${POLL_INTERVAL_SECONDS}s"
echo "  Max wait      = ${MAX_WAIT_SECONDS}s"

deadline=$(( $(date +%s) + MAX_WAIT_SECONDS ))
download_url=""

while :; do
    now="$(date +%s)"
    if [ "${now}" -ge "${deadline}" ]; then
        echo "ERROR: artifact ${ARTIFACT_NAME} did not appear within ${MAX_WAIT_SECONDS}s." >&2
        exit 1
    fi

    if body="$(curl -sS -f \
            -H "Authorization: Bearer ${SYSTEM_ACCESSTOKEN}" \
            -H "Accept: application/json" \
            "${url}" 2>/tmp/sql-host-poll-curl.err)"; then
        download_url="$(printf '%s' "${body}" | ARTIFACT_NAME="${ARTIFACT_NAME}" \
            python3 -c '
import json, os, sys
target = os.environ["ARTIFACT_NAME"]
data = json.load(sys.stdin)
for a in data.get("value", []):
    if a.get("name") == target:
        print(a.get("resource", {}).get("downloadUrl", ""))
        break
' 2>/dev/null || true)"
        if [ -n "${download_url}" ]; then
            echo "Endpoint sentinel ${ARTIFACT_NAME} found."
            break
        fi
    else
        echo "WARN: artifact list HTTP error: $(cat /tmp/sql-host-poll-curl.err)" >&2
    fi

    remaining=$(( deadline - now ))
    echo "$(date -u +%H:%M:%S) waiting for ${ARTIFACT_NAME}; ${remaining}s left."
    sleep "${POLL_INTERVAL_SECONDS}"
done

DOWNLOAD_DIR="${AGENT_TEMPDIRECTORY}/${ARTIFACT_NAME}"
mkdir -p "${DOWNLOAD_DIR}"
zip_path="${DOWNLOAD_DIR}/artifact.zip"

# Retry the download; transient 5xx / connection resets shouldn't fail the
# rendezvous after we already saw the artifact was published.
download_ok=0
for attempt in 1 2 3 4 5; do
    if curl -sS -f -L \
            -H "Authorization: Bearer ${SYSTEM_ACCESSTOKEN}" \
            -o "${zip_path}" \
            "${download_url}"; then
        download_ok=1
        break
    fi
    echo "WARN: artifact download attempt ${attempt} failed; retrying." >&2
    sleep $(( attempt * 5 ))
done
if [ "${download_ok}" -ne 1 ]; then
    echo "ERROR: failed to download ${ARTIFACT_NAME} after 5 attempts." >&2
    exit 1
fi

# DownloadUrl returns the artifact as a zip; the artifact root directory
# inside the zip equals the artifact name. Use python's zipfile module so
# we don't depend on `unzip` being installed on every agent image.
python3 -m zipfile -e "${zip_path}" "${DOWNLOAD_DIR}"

endpoint_file="$(find "${DOWNLOAD_DIR}" -type f -name endpoint.txt | head -1)"
if [ -z "${endpoint_file}" ]; then
    echo "ERROR: endpoint.txt not found inside ${ARTIFACT_NAME} artifact." >&2
    echo "--- contents ---" >&2
    find "${DOWNLOAD_DIR}" -type f -maxdepth 4 >&2 || true
    exit 1
fi

# endpoint.txt format: "HOST PORT DEADLINE_EPOCH" on a single line. The
# deadline is the UTC epoch second past which the SQL host will tear down.
# Refuse to proceed if we are within MIN_HOST_LIFE_REMAINING_SECONDS of it
# — connecting to a host that will disappear mid-test only buys us silent
# timeouts.
MIN_HOST_LIFE_REMAINING_SECONDS="${MIN_HOST_LIFE_REMAINING_SECONDS:-300}"
read -r host port deadline < "${endpoint_file}"
if [ -z "${host}" ] || [ -z "${port}" ]; then
    echo "ERROR: endpoint.txt is malformed: '$(cat "${endpoint_file}")'" >&2
    exit 1
fi

if [ -n "${deadline}" ]; then
    now="$(date +%s)"
    remaining=$(( deadline - now ))
    if [ "${remaining}" -lt "${MIN_HOST_LIFE_REMAINING_SECONDS}" ]; then
        echo "ERROR: SQL host endpoint expires in ${remaining}s (need >= ${MIN_HOST_LIFE_REMAINING_SECONDS}s)." >&2
        echo "  This usually means the SQL host hit MAX_LIFETIME_MINUTES while this test job was queued." >&2
        echo "  Failing fast rather than connecting to a host about to be torn down." >&2
        exit 1
    fi
    echo "SQL host has ${remaining}s of life remaining (well over ${MIN_HOST_LIFE_REMAINING_SECONDS}s threshold)."
fi

echo "Resolved SQL endpoint: host=${host} port=${port}"
echo "##vso[task.setvariable variable=DB_HOST]${host}"
echo "##vso[task.setvariable variable=DB_PORT]${port}"

# Stage SQL host's CA + server cert into Build.SourcesDirectory so the
# existing dockerentry trust setup (cp ca.crt /usr/local/share/ca-certificates;
# update-ca-certificates) trusts the cert this server is actually presenting.
# The colocated amd64 path generates these locally; the cross-pool path must
# instead consume the SQL host's, since that's the chain on the wire.
# Different distro dockerentry scripts reference mssql.crt vs mssql.pem
# interchangeably, so stage both.
ca_src="$(find "${DOWNLOAD_DIR}" -type f -name ca.crt | head -1)"
crt_src="$(find "${DOWNLOAD_DIR}" -type f -name mssql.crt | head -1)"
pem_src="$(find "${DOWNLOAD_DIR}" -type f -name mssql.pem | head -1)"
if [ -n "${ca_src}" ] && [ -n "${pem_src}" ]; then
    dest="${BUILD_SOURCESDIRECTORY:-${PWD}}"
    cp -f "${ca_src}" "${dest}/ca.crt"
    cp -f "${pem_src}" "${dest}/mssql.pem"
    if [ -n "${crt_src}" ]; then
        cp -f "${crt_src}" "${dest}/mssql.crt"
    else
        cp -f "${pem_src}" "${dest}/mssql.crt"
    fi
    echo "Staged SQL host CA + cert into ${dest}/{ca.crt,mssql.crt,mssql.pem}"
else
    echo "WARN: ca.crt or mssql.pem missing from ${ARTIFACT_NAME} artifact." >&2
    echo "      no-trust TLS handshake tests may fail." >&2
fi

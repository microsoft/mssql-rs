#!/usr/bin/env bash
# start.sh — boot a SQL Server docker container on this x64 1ES agent and
# write its private-VNet endpoint to a file that the calling YAML step
# publishes as the `sql-ready-<instanceId>` pipeline-artifact sentinel.
#
# Dependent ARM test jobs poll for that artifact (poll-for-endpoint.sh) to
# discover the endpoint without using ADO `dependsOn` on this job — the
# `dependsOn` path deadlocks because the test job would then have to wait
# for *this* job to fully complete (which it cannot, until the tests run
# and publish their teardown sentinels).
#
# Required env:
#   INSTANCE_ID                    Unique id (e.g. "shared", "shared_musl").
#   SQL_PASSWORD                   SA password (derived from build context
#                                  by derive-sql-password.sh; matches the
#                                  test job's derived value).
#   SQL_IMAGE_TAG                  e.g. "2025-latest".
#   BUILD_ARTIFACTSTAGINGDIRECTORY ADO-provided staging dir for artifacts.
#
# Optional env:
#   SQL_HOST_PORT                  Host port to publish (default 1433).
#   READINESS_TIMEOUT_SECONDS      Default 180.
#
# Side effects:
#   * Writes ${BUILD_ARTIFACTSTAGINGDIRECTORY}/sql-endpoint-${INSTANCE_ID}/endpoint.txt
#     containing "HOST PORT" on a single line.
#   * Also emits output variables sqlHost / sqlPort / sqlInstanceId for
#     diagnostics — they are not consumed by the test jobs.
#
# Notes:
#   * Cross-pool reachability assumes both 1ES pools live in a shared VNet
#     and an ARM agent can open TCP $SQL_HOST_PORT on the IP we write out.
#   * Container name and docker network are namespaced by INSTANCE_ID so
#     multiple SQL hosts can co-exist on the same agent (defensive — there
#     is typically one per VM).

set -euo pipefail

: "${INSTANCE_ID:?INSTANCE_ID is required}"
: "${SQL_PASSWORD:?SQL_PASSWORD is required}"
: "${SQL_IMAGE_TAG:?SQL_IMAGE_TAG is required}"
: "${BUILD_ARTIFACTSTAGINGDIRECTORY:?BUILD_ARTIFACTSTAGINGDIRECTORY is required}"

SQL_HOST_PORT="${SQL_HOST_PORT:-1433}"
READINESS_TIMEOUT_SECONDS="${READINESS_TIMEOUT_SECONDS:-180}"
# Wall-clock deadline beyond which the SQL host's wait-for-teardown.sh will
# give up and let teardown.sh destroy the container. We embed this in the
# endpoint sentinel so a late-arriving test job can refuse to connect to a
# host that is about to disappear (rather than silently timing out at SQL
# connect time, which is the failure mode the old design produced).
MAX_LIFETIME_MINUTES="${MAX_LIFETIME_MINUTES:-120}"

CONTAINER_NAME="sql-${INSTANCE_ID}"
NETWORK_NAME="testnet-${INSTANCE_ID}"
IMAGE="mcr.microsoft.com/mssql/server:${SQL_IMAGE_TAG}"

echo "=== sql-host/start.sh ==="
echo "  INSTANCE_ID  = ${INSTANCE_ID}"
echo "  IMAGE        = ${IMAGE}"
echo "  HOST_PORT    = ${SQL_HOST_PORT}"
echo "  CONTAINER    = ${CONTAINER_NAME}"

# 1. Reap any leftover from a prior attempt on this agent (defensive).
docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
docker network rm "${NETWORK_NAME}" >/dev/null 2>&1 || true

# 2. Resolve the agent's primary private IPv4 first — we need it both for
#    the endpoint sentinel and as an extra SAN on the SQL Server's cert
#    so that ARM test clients can verify the chain when connecting by IP.
SQL_HOST_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
if [ -z "${SQL_HOST_IP}" ]; then
    SQL_HOST_IP="$(ip -4 -o addr show scope global | awk 'NR==1 {split($4,a,"/"); print a[1]}')"
fi
if [ -z "${SQL_HOST_IP}" ]; then
    echo "ERROR: could not resolve a private IPv4 for this agent." >&2
    exit 1
fi
echo "Advertising SQL host endpoint ${SQL_HOST_IP}:${SQL_HOST_PORT}"

# 3. Generate certs locally (mssql.conf references mssql.pem / mssql.key).
#    We pass our private IP as an extra SAN so cross-pool clients that
#    connect by IP (rather than DNS name) can validate the chain.
EXTRA_IP_SAN="${SQL_HOST_IP}" scripts/generate_cert.sh
sudo chown 10001:0 mssql.* || true

# 4. Create an isolated user-defined network for this instance.
docker network create "${NETWORK_NAME}" >/dev/null

# 5. Boot the container.
docker run -d \
    --name "${CONTAINER_NAME}" \
    --hostname sql1 \
    --network "${NETWORK_NAME}" \
    -e ACCEPT_EULA=Y \
    -e "MSSQL_SA_PASSWORD=${SQL_PASSWORD}" \
    -p "${SQL_HOST_PORT}:1433" \
    -v "${PWD}/conf/mssql.conf:/var/opt/mssql/mssql.conf" \
    -v "${PWD}/mssql.pem:/etc/ssl/certs/mssql.pem" \
    -v "${PWD}/mssql.key:/etc/ssl/private/mssql.key" \
    -u 0:0 \
    "${IMAGE}" >/dev/null

# 6. Probe readiness with sqlcmd inside the container.
deadline=$(( $(date +%s) + READINESS_TIMEOUT_SECONDS ))
attempt=0
ready=0
while [ "$(date +%s)" -lt "${deadline}" ]; do
    attempt=$((attempt + 1))
    if docker exec "${CONTAINER_NAME}" /opt/mssql-tools18/bin/sqlcmd \
            -S localhost -U sa -P "${SQL_PASSWORD}" -C -Q "SELECT 1" \
            >/dev/null 2>&1; then
        ready=1
        echo "SQL Server ready after ${attempt} attempts."
        break
    fi
    sleep 2
done

if [ "${ready}" -ne 1 ]; then
    echo "ERROR: SQL Server did not become ready within ${READINESS_TIMEOUT_SECONDS}s." >&2
    echo "--- container logs (tail 200) ---" >&2
    docker logs --tail 200 "${CONTAINER_NAME}" >&2 || true
    exit 1
fi

# 7. Write endpoint to a file the calling YAML step publishes as the
#    `sql-ready-${INSTANCE_ID}` pipeline artifact. ARM test jobs poll for
#    this artifact (poll-for-endpoint.sh) to discover where to connect.
#    The third field is a wall-clock deadline (UTC epoch seconds) — the
#    time at which wait-for-teardown.sh will give up and trigger teardown.
#    Test jobs use it to fail fast if they queued so late that the SQL
#    host is about to be torn down.
#    We also stage ca.crt + mssql.pem so cross-pool clients trust this
#    SQL Server's self-signed cert chain (TLS handshake tests need it).
ENDPOINT_DIR="${BUILD_ARTIFACTSTAGINGDIRECTORY}/sql-endpoint-${INSTANCE_ID}"
mkdir -p "${ENDPOINT_DIR}"
deadline_epoch=$(( $(date +%s) + MAX_LIFETIME_MINUTES * 60 ))
printf '%s %s %s\n' "${SQL_HOST_IP}" "${SQL_HOST_PORT}" "${deadline_epoch}" \
    > "${ENDPOINT_DIR}/endpoint.txt"
# mssql.crt and mssql.pem are identical (generate_cert.sh produces .crt and
# copies it to .pem). Different distro dockerentry scripts reference one or
# the other for `openssl verify`, so stage both.
sudo cp -f ca.crt mssql.crt mssql.pem "${ENDPOINT_DIR}/" \
    || cp -f ca.crt mssql.crt mssql.pem "${ENDPOINT_DIR}/"
sudo chmod a+r "${ENDPOINT_DIR}/ca.crt" "${ENDPOINT_DIR}/mssql.crt" "${ENDPOINT_DIR}/mssql.pem" || true
echo "Wrote endpoint sentinel payload to ${ENDPOINT_DIR}/endpoint.txt"
echo "  endpoint = ${SQL_HOST_IP}:${SQL_HOST_PORT}"
echo "  deadline = $(date -u -d @${deadline_epoch} +%FT%TZ) (epoch ${deadline_epoch})"
echo "  staged   = ca.crt, mssql.crt, mssql.pem"

# 8. Also expose as job output variables. Not consumed by the test jobs
#    (we deliberately avoid the `dependsOn` deadlock) but useful for
#    inspecting this job's timeline output in the ADO UI.
echo "##vso[task.setvariable variable=sqlHost;isOutput=true]${SQL_HOST_IP}"
echo "##vso[task.setvariable variable=sqlPort;isOutput=true]${SQL_HOST_PORT}"
echo "##vso[task.setvariable variable=sqlInstanceId;isOutput=true]${INSTANCE_ID}"

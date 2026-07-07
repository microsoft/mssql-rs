#!/usr/bin/env bash
# teardown.sh — idempotent stop/remove of the SQL container and its docker
# network. Designed for an always() step so it must succeed regardless of
# whether start.sh got far enough to create either resource.
#
# Required env:
#   INSTANCE_ID   Same value used by start.sh.

set -u

: "${INSTANCE_ID:?INSTANCE_ID is required}"

CONTAINER_NAME="sql-${INSTANCE_ID}"
NETWORK_NAME="testnet-${INSTANCE_ID}"

echo "=== sql-host/teardown.sh (instance=${INSTANCE_ID}) ==="

if docker ps -a --format '{{.Names}}' | grep -qxF "${CONTAINER_NAME}"; then
    echo "--- container logs (tail 200) before removal ---"
    docker logs --tail 200 "${CONTAINER_NAME}" 2>&1 || true
    docker stop "${CONTAINER_NAME}" >/dev/null 2>&1 || true
    docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
    echo "Container ${CONTAINER_NAME} removed."
else
    echo "Container ${CONTAINER_NAME} not present; nothing to stop."
fi

if docker network ls --format '{{.Name}}' | grep -qxF "${NETWORK_NAME}"; then
    docker network rm "${NETWORK_NAME}" >/dev/null 2>&1 || true
    echo "Network ${NETWORK_NAME} removed."
else
    echo "Network ${NETWORK_NAME} not present; nothing to remove."
fi

exit 0

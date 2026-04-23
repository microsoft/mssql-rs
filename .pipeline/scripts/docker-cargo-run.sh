#!/bin/bash
# docker-cargo-run.sh - Wrapper for docker run that injects CARGO registry tokens
#
# Usage: docker-cargo-run.sh [docker-run-args...] <image> <command> [command-args...]
#
# This script prepends the cargo registry environment variables to any docker run command.
# It also prints debug info about the token status before running.
#
# Example:
#   .pipeline/scripts/docker-cargo-run.sh --rm -v $PWD:/workspace -w /workspace \
#     my-image:latest /workspace/scripts/build.sh

set -euo pipefail

# Debug: Echo cargo registry token status (masked for security)
echo "=== Cargo Registry Token Status ==="
echo "CARGO_REGISTRIES_MSSQL_RS_PUBLIC_TOKEN is set: $([ -n "${CARGO_REGISTRIES_MSSQL_RS_PUBLIC_TOKEN:-}" ] && echo 'YES' || echo 'NO')"
echo "CARGO_REGISTRIES_MSSQL_RS_PUBLIC_TOKEN length: ${#CARGO_REGISTRIES_MSSQL_RS_PUBLIC_TOKEN}"
echo "CARGO_REGISTRIES_MSSQL_RS_TOKEN is set: $([ -n "${CARGO_REGISTRIES_MSSQL_RS_TOKEN:-}" ] && echo 'YES' || echo 'NO')"
echo "CARGO_REGISTRIES_MSSQL_RS_TOKEN length: ${#CARGO_REGISTRIES_MSSQL_RS_TOKEN}"
echo "===================================="

# Check disk space and prune Docker images if free space is below 10%
# Check both root and the Docker data-root partition
check_and_prune() {
  local mount_label="$1"
  local mount_path="$2"
  local used_pct
  used_pct=$(df "$mount_path" 2>/dev/null | awk 'NR==2 {print $5}' | tr -d '%')
  if [ -z "$used_pct" ]; then
    return
  fi
  echo "=== Disk Space Check ($mount_label): ${used_pct}% used ==="
  if [ "$used_pct" -gt 75 ]; then
    echo "Warning: $mount_label disk usage is above 75% (free space below 25%). Pruning Docker images..."
    docker image prune -af
    echo "Disk space after prune:"
    df -h "$mount_path"
  fi
}

DOCKER_ROOT=$(docker info --format '{{.DockerRootDir}}' 2>/dev/null || echo "/var/lib/docker")
check_and_prune "root (/)" "/"
check_and_prune "Docker data-root ($DOCKER_ROOT)" "$DOCKER_ROOT"

exec docker run \
  -e CARGO_REGISTRIES_MSSQL_RS_PUBLIC_TOKEN="$CARGO_REGISTRIES_MSSQL_RS_PUBLIC_TOKEN" \
  -e CARGO_REGISTRIES_MSSQL_RS_PUBLIC_CREDENTIAL_PROVIDER="cargo:token" \
  -e CARGO_REGISTRIES_MSSQL_RS_TOKEN="$CARGO_REGISTRIES_MSSQL_RS_TOKEN" \
  -e CARGO_REGISTRIES_MSSQL_RS_CREDENTIAL_PROVIDER="cargo:token" \
  "$@"

#!/bin/bash
# docker-cargo-run.sh - Forward optional Cargo feed credentials on non-PR runs
#
# Usage: docker-cargo-run.sh [docker-run-args...] <image> <command> [command-args...]
#
# BUILD_REASON=PullRequest disables feed token/provider forwarding. Azure Pipelines
# supplies BUILD_REASON automatically. Missing/empty/unresolved tokens are omitted.
# Callers must not map feed secrets into PR steps: this is not a security sandbox.
#
# Example:
#   .pipeline/scripts/docker-cargo-run.sh --rm -v $PWD:/workspace -w /workspace \
#     my-image:latest /workspace/scripts/build.sh

set -euo pipefail

cargo_env=()
for registry in MSSQL_RS_PUBLIC MSSQL_RS; do
  token_var="CARGO_REGISTRIES_${registry}_TOKEN"
  provider_var="CARGO_REGISTRIES_${registry}_CREDENTIAL_PROVIDER"
  token="${!token_var:-}"
  if [[ "${BUILD_REASON:-}" != "PullRequest" && -n "$token" && "$token" != '$('*')' ]]; then
    # Pass the variable name rather than putting its value in Docker's arguments.
    cargo_env+=(-e "$token_var" -e "$provider_var=cargo:token")
  else
    unset "$token_var" "$provider_var"
  fi
done
unset token

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

exec docker run "${cargo_env[@]}" "$@"

#!/bin/bash
# SANDBOX / TEST-ONLY host wrapper.
# Runs the in-container mock wheel build (build-mock-python-wheel-in-container.sh)
# via docker-cargo-run.sh, then verifies at least one wheel landed in the output
# directory. Used by all Linux build jobs (manylinux + musllinux, x64 + arm64).
#
# Env:
#   CONTAINER_IMAGE        Build image (e.g. .../manylinux_2_34_x86_64_rust:latest). Required.
#   OB_OUTPUTDIRECTORY     Job artifact staging dir; wheels are written to $OB_OUTPUTDIRECTORY/wheels. Required.
#   BUILD_SOURCESDIRECTORY Repo checkout mounted into the container as /workspace. Required.
#   MATURIN_FEATURES       Optional cargo features (e.g. vendored-openssl for musllinux).
set -euo pipefail

: "${CONTAINER_IMAGE:?CONTAINER_IMAGE is required}"
: "${OB_OUTPUTDIRECTORY:?OB_OUTPUTDIRECTORY is required}"
: "${BUILD_SOURCESDIRECTORY:?BUILD_SOURCESDIRECTORY is required}"

WHEELS_DIR="${OB_OUTPUTDIRECTORY}/wheels"
echo "Building mock wheel in container: ${CONTAINER_IMAGE}"
mkdir -p "$WHEELS_DIR"

FEATURE_ENV=()
if [ -n "${MATURIN_FEATURES:-}" ]; then
  FEATURE_ENV=(-e "MATURIN_FEATURES=${MATURIN_FEATURES}")
fi

bash .pipeline/scripts/docker-cargo-run.sh --rm \
  -v "${BUILD_SOURCESDIRECTORY}:/workspace" \
  -v "${WHEELS_DIR}:/workspace/target/wheels" \
  -e "WORKSPACE_DIR=/workspace" \
  -e "OUTPUT_DIR=/workspace/target/wheels" \
  "${FEATURE_ENV[@]}" \
  "${CONTAINER_IMAGE}" \
  bash /workspace/scripts/build-mock-python-wheel-in-container.sh

sudo chown -R "$(whoami):$(whoami)" "${BUILD_SOURCESDIRECTORY}/target" || true

WHEEL_COUNT=$(find "$WHEELS_DIR" -type f -name '*.whl' | wc -l)
if [ "$WHEEL_COUNT" -eq 0 ]; then
  echo "ERROR: No mock wheels were produced!"
  exit 1
fi
echo "$WHEEL_COUNT wheel(s) built."
ls -lh "$WHEELS_DIR"

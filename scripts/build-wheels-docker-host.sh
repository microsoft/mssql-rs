#!/bin/bash
set -eu

# Script to build Python wheels using Docker containers on the pipeline host
# This runs on the Azure DevOps agent, not inside the container

# Parameters
OS_TYPE="${1:-Linux}"           # Linux or Alpine
ARCH="${2:-x64}"                # x64 or ARM64/arm64
SOURCES_DIR="${3:-$(pwd)}"
STAGING_DIR="${4:-$(pwd)/target/wheels}"
# manylinux glibc floor: 2_28 (AlmaLinux 8, libssl.so.1.1) or 2_34 (AlmaLinux 9,
# libssl.so.3). Ignored for Alpine. Default 2_28 — broadest reach and the tag
# mssql-python's own Linux wheels carry.
MANYLINUX_FLAVOR="${5:-2_28}"

echo "Building Python wheels using container..."
echo "OS Type: $OS_TYPE"
echo "Architecture: $ARCH"
echo "Source Directory: $SOURCES_DIR"
echo "Output Directory: $STAGING_DIR"

# Determine container image based on OS type and architecture
if [ "$OS_TYPE" = "Linux" ]; then
  case "$MANYLINUX_FLAVOR" in
    2_28|2_34) ;;
    *) echo "ERROR: Unsupported MANYLINUX_FLAVOR: $MANYLINUX_FLAVOR (expected 2_28 or 2_34)"; exit 1 ;;
  esac
  echo "manylinux flavor: $MANYLINUX_FLAVOR"
  if [ "$ARCH" = "x64" ]; then
    CONTAINER_IMAGE="tdslibrs.azurecr.io/python-build/manylinux_${MANYLINUX_FLAVOR}_x86_64_rust:latest"
    MATURIN_COMPATIBILITY="manylinux_${MANYLINUX_FLAVOR}_x86_64"
  elif [ "$ARCH" = "ARM64" ] || [ "$ARCH" = "arm64" ]; then
    CONTAINER_IMAGE="tdslibrs.azurecr.io/python-build/manylinux_${MANYLINUX_FLAVOR}_aarch64_rust:latest"
    MATURIN_COMPATIBILITY="manylinux_${MANYLINUX_FLAVOR}_aarch64"
  else
    echo "ERROR: Unsupported architecture: $ARCH"
    exit 1
  fi
  SHELL_CMD="bash"
elif [ "$OS_TYPE" = "Alpine" ]; then
  if [ "$ARCH" = "x64" ]; then
    CONTAINER_IMAGE="tdslibrs.azurecr.io/python-build/musllinux_1_2_x86_64_rust:latest"
    MATURIN_COMPATIBILITY="musllinux_1_2_x86_64"
  elif [ "$ARCH" = "ARM64" ] || [ "$ARCH" = "arm64" ]; then
    CONTAINER_IMAGE="tdslibrs.azurecr.io/python-build/musllinux_1_2_aarch64_rust:latest"
    MATURIN_COMPATIBILITY="musllinux_1_2_aarch64"
  else
    echo "ERROR: Unsupported architecture: $ARCH"
    exit 1
  fi
  SHELL_CMD="sh"
else
  echo "ERROR: Unsupported OS type: $OS_TYPE"
  exit 1
fi

echo "Using container: $CONTAINER_IMAGE"
echo "Maturin compatibility tag: $MATURIN_COMPATIBILITY"
echo "Shell command: $SHELL_CMD"

# Login to ACR
echo "Logging into Azure Container Registry..."
az acr login -n tdslibrs

# Create output directory
mkdir -p "$STAGING_DIR"

# Run build in container
echo "Running wheel build in container..."
docker run --rm \
  -v "$SOURCES_DIR:/workspace" \
  -v "$STAGING_DIR:/workspace/target/wheels" \
  -e "WORKSPACE_DIR=/workspace" \
  -e "OUTPUT_DIR=/workspace/target/wheels" \
  -e "MATURIN_COMPATIBILITY=$MATURIN_COMPATIBILITY" \
  "$CONTAINER_IMAGE" \
  $SHELL_CMD /workspace/scripts/build-python-wheels-in-container.sh

echo ""
echo "✅ Wheels built successfully!"
echo "Output directory: $STAGING_DIR"
ls -lh "$STAGING_DIR"

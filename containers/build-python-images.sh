#!/bin/bash
set -e

# Build pre-configured Python wheel build images with Rust and maturin pre-installed
# These images extend the official PyPA manylinux/musllinux images from ACR

ACR_NAME="tdslibrs"
ACR_REGISTRY="tdslibrs.azurecr.io"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "======================================================"
echo "Building Python wheel build images with Rust pre-installed"
echo "======================================================"
echo ""

# Login to ACR to pull base images
echo "==> Logging into Azure Container Registry..."
az acr login --name "$ACR_NAME"
echo "✅ ACR login successful"
echo ""

cd "$SCRIPT_DIR"

# Build manylinux_2_28 x64 image (glibc 2.28, OpenSSL 1.1)
echo "==> Building manylinux_2_28 x64 image..."
docker build \
    -f Dockerfile.PythonBuild.manylinux_2_28.x64 \
    -t "${ACR_REGISTRY}/python-build/manylinux_2_28_x86_64_rust:latest" \
    .
echo "✅ manylinux_2_28 x64 image built successfully"
echo ""

# Build manylinux_2_28 ARM64 image (glibc 2.28, OpenSSL 1.1)
echo "==> Building manylinux_2_28 ARM64 image..."
docker build \
    -f Dockerfile.PythonBuild.manylinux_2_28.arm64 \
    -t "${ACR_REGISTRY}/python-build/manylinux_2_28_aarch64_rust:latest" \
    .
echo "✅ manylinux_2_28 ARM64 image built successfully"
echo ""

# Build manylinux x64 image
echo "==> Building manylinux x64 image..."
docker build \
    -f Dockerfile.PythonBuild.manylinux.x64 \
    -t "${ACR_REGISTRY}/python-build/manylinux_2_34_x86_64_rust:latest" \
    .
echo "✅ manylinux x64 image built successfully"
echo ""

# Build manylinux ARM64 image
echo "==> Building manylinux ARM64 image..."
docker build \
    -f Dockerfile.PythonBuild.manylinux.arm64 \
    -t "${ACR_REGISTRY}/python-build/manylinux_2_34_aarch64_rust:latest" \
    .
echo "✅ manylinux ARM64 image built successfully"
echo ""

# Build musllinux x64 image
echo "==> Building musllinux x64 image..."
docker build \
    -f Dockerfile.PythonBuild.musllinux.x64 \
    -t "${ACR_REGISTRY}/python-build/musllinux_1_2_x86_64_rust:latest" \
    .
echo "✅ musllinux x64 image built successfully"
echo ""

# Build musllinux ARM64 image
echo "==> Building musllinux ARM64 image..."
docker build \
    -f Dockerfile.PythonBuild.musllinux.arm64 \
    -t "${ACR_REGISTRY}/python-build/musllinux_1_2_aarch64_rust:latest" \
    .
echo "✅ musllinux ARM64 image built successfully"
echo ""

echo "======================================================"
echo "✅ All images built successfully!"
echo "======================================================"
echo ""
echo "Built images:"
echo "  - ${ACR_REGISTRY}/python-build/manylinux_2_28_x86_64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/manylinux_2_28_aarch64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/manylinux_2_34_x86_64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/manylinux_2_34_aarch64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/musllinux_1_2_x86_64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/musllinux_1_2_aarch64_rust:latest"
echo ""
echo "To push to ACR, run:"
echo "  ./push-python-images.sh"

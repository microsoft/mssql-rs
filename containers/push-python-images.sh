#!/bin/bash
set -e

# Push pre-configured Python wheel build images to Azure Container Registry
# Run build-python-images.sh first to build the images

ACR_REGISTRY="tdslibrs.azurecr.io"

echo "======================================================"
echo "Pushing Python wheel build images to ACR"
echo "======================================================"
echo ""
echo "Registry: ${ACR_REGISTRY}"
echo ""

# Check if logged into ACR
if ! az account show &>/dev/null; then
    echo "Not logged into Azure. Please run 'az login' first."
    exit 1
fi

# Login to ACR (skip in CI/CD pipelines - authentication is handled externally)
if [ -z "$CI" ] && [ -z "$GITHUB_ACTIONS" ] && [ -z "$AZURE_PIPELINES" ]; then
    echo "==> Logging into Azure Container Registry..."
    az acr login --name tdslibrs
    echo ""
else
    echo "==> Running in CI/CD pipeline, skipping ACR login..."
    echo ""
fi


# Push manylinux_2_28 x64 image
echo "==> Pushing manylinux_2_28 x64 image..."
docker push "${ACR_REGISTRY}/python-build/manylinux_2_28_x86_64_rust:latest"
echo "✅ manylinux_2_28 x64 image pushed"
echo ""

# Push manylinux_2_28 ARM64 image
echo "==> Pushing manylinux_2_28 ARM64 image..."
docker push "${ACR_REGISTRY}/python-build/manylinux_2_28_aarch64_rust:latest"
echo "✅ manylinux_2_28 ARM64 image pushed"
echo ""

# Push manylinux x64 image
echo "==> Pushing manylinux x64 image..."
docker push "${ACR_REGISTRY}/python-build/manylinux_2_34_x86_64_rust:latest"
echo "✅ manylinux x64 image pushed"
echo ""

# Push manylinux ARM64 image
echo "==> Pushing manylinux ARM64 image..."
docker push "${ACR_REGISTRY}/python-build/manylinux_2_34_aarch64_rust:latest"
echo "✅ manylinux ARM64 image pushed"
echo ""

# Push musllinux x64 image
echo "==> Pushing musllinux x64 image..."
docker push "${ACR_REGISTRY}/python-build/musllinux_1_2_x86_64_rust:latest"
echo "✅ musllinux x64 image pushed"
echo ""

# Push musllinux ARM64 image
echo "==> Pushing musllinux ARM64 image..."
docker push "${ACR_REGISTRY}/python-build/musllinux_1_2_aarch64_rust:latest"
echo "✅ musllinux ARM64 image pushed"
echo ""

echo "======================================================"
echo "✅ All images pushed successfully!"
echo "======================================================"
echo ""
echo "Images available in ACR:"
echo "  - ${ACR_REGISTRY}/python-build/manylinux_2_28_x86_64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/manylinux_2_28_aarch64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/manylinux_2_34_x86_64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/manylinux_2_34_aarch64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/musllinux_1_2_x86_64_rust:latest"
echo "  - ${ACR_REGISTRY}/python-build/musllinux_1_2_aarch64_rust:latest"

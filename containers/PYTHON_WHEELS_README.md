# Python Wheel Building with Containers

This directory contains scripts and Dockerfiles for building Python wheels in a consistent, reproducible manner.

## Overview

We build Python wheels inside **manylinux** and **musllinux** containers to:
- ✅ Avoid GitHub API rate limits (no need to download Python from GitHub)
- ✅ Ensure binary compatibility across Linux distributions
- ✅ Use official PyPA (Python Packaging Authority) build environments
- ✅ Build one `cp310-abi3` wheel for CPython 3.10 and later
- ✅ Support both x64 and ARM64 architectures

## Container Images

All images are hosted in your Azure Container Registry (`tdslibrs.azurecr.io`) to avoid Docker Hub rate limits.

### 🚀 Pre-built Images with Rust (Recommended for CI/CD)
These custom images extend official PyPA images with **Rust toolchain and maturin pre-installed** for faster builds.

#### Linux (glibc-based - Ubuntu, RHEL, Debian, etc.)
- **x64**: `tdslibrs.azurecr.io/python-build/manylinux_2_28_x86_64_rust:latest`
- **ARM64**: `tdslibrs.azurecr.io/python-build/manylinux_2_28_aarch64_rust:latest`

#### Alpine (musl-based)
- **x64**: `tdslibrs.azurecr.io/python-build/musllinux_1_2_x86_64_rust:latest`
- **ARM64**: `tdslibrs.azurecr.io/python-build/musllinux_1_2_aarch64_rust:latest`

**Pre-installed software:**
- ✅ Rust toolchain (stable, via rustup)
- ✅ maturin (Python-to-Rust build tool)
- ✅ OpenSSL development libraries
- ✅ pkg-config
- ✅ All Python versions (3.10-3.14)
- ✅ Build tools and compilers

### Vanilla PyPA Images (Base images without Rust)
Use these only if you need stock PyPA images without customization.

#### Linux (glibc-based)
- **x64**: `tdslibrs.azurecr.io/python-build/manylinux_2_28_x86_64:latest`
- **ARM64**: `tdslibrs.azurecr.io/python-build/manylinux_2_28_aarch64:latest`

#### Alpine (musl-based)
- **x64**: `tdslibrs.azurecr.io/python-build/musllinux_1_2_x86_64:latest`
- **ARM64**: `tdslibrs.azurecr.io/python-build/musllinux_1_2_aarch64:latest`

### MacOS
- Built natively on MacOS agents (no container)

### Windows
- ❌ **Not supported** - Windows wheel builds are disabled

## Setup Instructions

### Option 1: Build Custom Images with Rust (Recommended)

Build the pre-configured images with Rust and maturin pre-installed for **faster CI/CD pipelines**:

```bash
cd containers

# Build all 4 images (manylinux + musllinux, x64 + ARM64)
./build-python-images.sh

# Push images to Azure Container Registry
./push-python-images.sh
```

**What gets pre-installed:**
- Rust toolchain (stable)
- maturin (Python wheel builder)
- OpenSSL development libraries
- pkg-config
- All system dependencies needed for building

**Benefits:**
- ⚡ **5-10x faster builds** - no runtime installation of Rust/maturin
- 🔒 **More reliable** - no dependency on rustup.rs availability
- 📦 **Better caching** - dependencies baked into image layers

**When to rebuild:**
- When you need to update Rust version
- When new Python versions are added to base PyPA images (quarterly)
- When you want to update maturin

### Option 2: Import Vanilla PyPA Images (Base Images Only)

Run this to pull official PyPA images without customization:

```bash
cd containers
chmod +x import-python-build-images.sh
./import-python-build-images.sh
```

This imports stock images from `quay.io/pypa` without pre-installed Rust/maturin. The build script will install these at runtime (slower).

**Note**: Recommended to re-import monthly to get PyPA updates.

### 2. Build Wheels Locally (Optional)

To test wheel building locally using the pre-built Rust images:

```bash
# For x64 Linux (manylinux with Rust pre-installed)
docker run --rm \
  -v "$(pwd):/workspace" \
  -v "$(pwd)/target/wheels:/workspace/target/wheels" \
  tdslibrs.azurecr.io/python-build/manylinux_2_28_x86_64_rust:latest \
  bash /workspace/scripts/build-python-wheels-in-container.sh

# For x64 Alpine (musllinux with Rust pre-installed)
docker run --rm \
  -v "$(pwd):/workspace" \
  -v "$(pwd)/target/wheels:/workspace/target/wheels" \
  tdslibrs.azurecr.io/python-build/musllinux_1_2_x86_64_rust:latest \
  sh /workspace/scripts/build-python-wheels-in-container.sh
```

Wheels will be created in `target/wheels/`.

## Pipeline Integration

The Azure DevOps pipeline automatically builds wheels in containers:

**Template**: `.pipeline/templates/build-python-wheels-template.yml`

**Usage**:
```yaml
- template: templates/build-python-wheels-template.yml
  parameters:
    osType: Linux        # Linux, Alpine, or MacOS
    architecture: x64    # x64 or ARM64
```

**What it does**:
1. Logs into ACR
2. Pulls the appropriate container image
3. Mounts source code into the container
4. Runs `build-python-wheels-in-container.sh`
5. Builds one `cp310-abi3` wheel for the target platform
6. Publishes wheels as build artifacts

## About manylinux and musllinux

**manylinux** and **musllinux** are standardized Docker images maintained by the Python Packaging Authority (PyPA):

- **Purpose**: Build portable binary wheels that work across many Linux distributions
- **Standards**: 
  - `manylinux_2_28`: Compatible with glibc 2.28+ (Ubuntu 20.04+, RHEL 8+, Debian 10+)
  - `musllinux_1_2`: Compatible with musl 1.2+ (Alpine 3.13+)
- **Pre-installed**: Multiple Python versions (3.8-3.14), compilers, build tools
- **Official**: Maintained by PyPA at https://github.com/pypa/manylinux

**Benefits**:
- ✅ Binary compatibility guaranteed
- ✅ No need to install Python versions manually
- ✅ Audited with `auditwheel` to verify portability
- ✅ Used by major Python packages (numpy, pandas, cryptography, etc.)

## Maintenance

### Update Custom Build Images

When you need to refresh the Rust toolchain, maturin, or base PyPA images:

```bash
cd containers

# Rebuild all custom images with latest base images and tools
./build-python-images.sh

# Push updated images to ACR
./push-python-images.sh
```

**When to update:**
- Quarterly - to get latest Python versions from PyPA base images
- When new Rust version is needed
- When maturin needs updating
- After security patches in base images

### Update Vanilla PyPA Images

Re-import stock PyPA images monthly or when new Python versions are released:

```bash
cd containers
./import-python-build-images.sh
```

### Image Architecture

```
Custom Images (_rust suffix):
  ├── Base: quay.io/pypa/manylinux_2_28_x86_64:latest
  └── Adds:
      ├── Rust toolchain (stable)
      ├── maturin
      ├── OpenSSL dev libraries
      └── pkg-config

Vanilla Images (no suffix):
  └── Direct import from quay.io/pypa
      └── No customization
```

### Modify the Stable ABI Floor

The wheel uses the CPython 3.10 stable ABI. To change that floor, update the
PyO3 `abi3-py310` feature and all `cp310-abi3` wheel selection and validation
logic together.

## Troubleshooting

### Container image not found

**For custom Rust images:**
```bash
# Build the custom images locally
cd containers
./build-python-images.sh
./push-python-images.sh
```

**For vanilla PyPA images:**
```bash
# Re-import base images
cd containers
./import-python-build-images.sh
```

### Rust installation is slow in CI
Switch to using the custom `_rust` images which have Rust pre-installed:
- Use: `manylinux_2_28_x86_64_rust:latest` 
- Instead of: `manylinux_2_28_x86_64:latest`

### Python version not building
Check the container has that Python version:
```bash
docker run --rm tdslibrs.azurecr.io/python-build/manylinux_2_28_x86_64:latest \
  ls -la /opt/python
```

### ACR authentication fails
```bash
az acr login -n tdslibrs
```

## References

- [PyPA manylinux](https://github.com/pypa/manylinux)
- [PEP 600 - manylinux_2_28](https://peps.python.org/pep-0600/)
- [PEP 656 - musllinux](https://peps.python.org/pep-0656/)
- [Maturin documentation](https://www.maturin.rs/)

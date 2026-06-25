# Python Wheel Building with Containers

This directory contains scripts and Dockerfiles for building Python wheels in a consistent, reproducible manner.

## Overview

We build Python wheels inside **manylinux** and **musllinux** containers to:
- ✅ Avoid GitHub API rate limits (no need to download Python from GitHub)
- ✅ Ensure binary compatibility across Linux distributions
- ✅ Use official PyPA (Python Packaging Authority) build environments
- ✅ Build for multiple Python versions (3.10-3.14) in one step
- ✅ Support both x64 and ARM64 architectures

## Container Images

All images are hosted in your Azure Container Registry (`tdslibrs.azurecr.io`) to avoid Docker Hub rate limits.

### 🚀 Pre-built Images with Rust (Recommended for CI/CD)
These custom images extend official PyPA images with **Rust toolchain and maturin pre-installed** for faster builds.

We maintain **two manylinux flavors** so we can ship a `.so` whose ABI honestly
matches the wheel tag. mssql-py-core wheels are repackaged into mssql-python,
which targets both OpenSSL 1.1 and OpenSSL 3 deployments, so we build for both:

#### Linux glibc 2.28 / OpenSSL 1.1 (AlmaLinux 8 base)
Wheel tag: `manylinux_2_28_<arch>`. Links to `libssl.so.1.1`, requires GLIBC ≤ 2.28.
Targets RHEL 8, Ubuntu 20.04, Debian 11, Amazon Linux 2.
- **x64**: `tdslibrs.azurecr.io/python-build/manylinux_2_28_x86_64_rust:latest`
- **ARM64**: `tdslibrs.azurecr.io/python-build/manylinux_2_28_aarch64_rust:latest`

#### Linux glibc 2.34 / OpenSSL 3 (AlmaLinux 9 base)
Wheel tag: `manylinux_2_34_<arch>`. Links to `libssl.so.3`, requires GLIBC ≤ 2.34.
Targets RHEL 9+, Ubuntu 22.04+, Debian 12+, Fedora 36+, Amazon Linux 2023.
- **x64**: `tdslibrs.azurecr.io/python-build/manylinux_2_34_x86_64_rust:latest`
- **ARM64**: `tdslibrs.azurecr.io/python-build/manylinux_2_34_aarch64_rust:latest`

#### Alpine (musl-based)
Wheel tag: `musllinux_1_2_<arch>`.
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
    osType: Linux           # Linux, Alpine, MacOS, or Windows
    architecture: x64       # x64 or ARM64
    manylinuxFlavor: '2_28' # Only honored for osType=Linux: '2_28' or '2_34'
```

For Linux, call the template **twice** per arch (once with `manylinuxFlavor: '2_28'`,
once with `'2_34'`) to produce both flavors. The published artifacts include the
flavor in their name (`wheels_Linux_<arch>_manylinux_2_28`,
`wheels_Linux_<arch>_manylinux_2_34`) so they don't collide.

**What it does**:
1. Logs into ACR
2. Pulls the appropriate container image
3. Mounts source code into the container
4. Runs `build-python-wheels-in-container.sh`
5. Builds wheels for Python 3.10, 3.11, 3.12, 3.13, 3.14
6. Publishes wheels as build artifacts

## About manylinux and musllinux

**manylinux** and **musllinux** are standardized Docker images maintained by the Python Packaging Authority (PyPA):

- **Purpose**: Build portable binary wheels that work across many Linux distributions
- **Standards**: 
  - `manylinux_2_28`: Compatible with glibc 2.28+ (RHEL 8, Ubuntu 20.04, Debian 11, Amazon Linux 2). Links against OpenSSL 1.1.
  - `manylinux_2_34`: Compatible with glibc 2.34+ (RHEL 9+, Ubuntu 22.04+, Debian 12+). Links against OpenSSL 3.
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

### Modify Python Versions

Edit `.pipeline/templates/build-python-wheels-template.yml`:

```yaml
parameters:
- name: pythonVersions
  type: object
  default:
  - '3.10'
  - '3.11'
  - '3.12'
  - '3.13'
  - '3.14'  # Add/remove versions here
```

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

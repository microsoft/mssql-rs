# Python wheel build containers

Linux wheels are built in PyPA-based manylinux and musllinux containers for
x64 and ARM64. Each build produces one `cp310-abi3` wheel, not one wheel per
Python minor version. Windows x64/ARM64 and macOS universal2 wheels are built
natively. See the [release guide](../docs/release-management.md#wheel-matrix-9-wheels)
for the complete wheel matrix and platform requirements.

## Images

The shared wheel template defaults to `ghcr.io/microsoft/mssql-rs`; OneBranch
callers select `tdslibrs.azurecr.io`. ACR access requires Azure authentication.

`build-python-images.sh` and `push-python-images.sh` build and publish these ACR
images with Rust, maturin, OpenSSL development libraries, and `pkg-config`:

| Target | Image under `tdslibrs.azurecr.io/python-build/` |
|---|---|
| Linux x64 | `manylinux_2_34_x86_64_rust:latest` |
| Linux ARM64 | `manylinux_2_34_aarch64_rust:latest` |
| Alpine x64 | `musllinux_1_2_x86_64_rust:latest` |
| Alpine ARM64 | `musllinux_1_2_aarch64_rust:latest` |

These manylinux images use glibc 2.34 and OpenSSL 3. The separate glibc 2.28
release track uses the older compatibility build path; these images do not
produce glibc 2.28 builds.

`import-python-build-images.sh` mirrors stock manylinux 2.28 and musllinux 1.2
images under `tdslibrs.azurecr.io/import/python-build/`. These are base images,
not replacements for the `_rust` images: the wheel-build script requires Rust
and maturin to be installed already. CI image synchronization also supplies the
manylinux 2.34 bases used by the Dockerfiles.

## Build and publish images

With Docker, Azure CLI, and permission to push to the ACR, run:

```bash
cd containers
./build-python-images.sh
./push-python-images.sh
```

Rebuild when the toolchain, maturin, or base-image dependencies change.

## Build wheels locally

Run from the **repository root**, using an image matching the host architecture:

```bash
az acr login --name tdslibrs

# Linux x64
docker run --rm \
  -v "$(pwd):/workspace" \
  tdslibrs.azurecr.io/python-build/manylinux_2_34_x86_64_rust:latest \
  bash /workspace/scripts/build-python-wheels-in-container.sh

# Alpine x64
docker run --rm \
  -v "$(pwd):/workspace" \
  tdslibrs.azurecr.io/python-build/musllinux_1_2_x86_64_rust:latest \
  bash /workspace/scripts/build-python-wheels-in-container.sh
```

Wheels are written to `target/wheels/`. This script builds the native extension;
it does not perform the release pipeline's ODBC-driver injection, wheel repair,
or clean-install checks. OpenSSL remains OS-provided rather than vendored.

## Pipeline integration

The [shared template](../.pipeline/templates/build-python-wheels-template.yml)
selects container builds for Linux/Alpine and native builds for Windows/macOS.
For example, from a pipeline under `.pipeline/`:

```yaml
- template: templates/build-python-wheels-template.yml
  parameters:
    osType: Linux        # Linux, Alpine, MacOS, or Windows
    architecture: x64    # x64 or ARM64
```

Authenticate to the selected registry before pulling private images. The template
builds the wheel and optionally publishes artifacts; its callers own registry
login and the remaining release steps.

## Stable ABI floor

To change the CPython 3.10 stable ABI floor, update PyO3's `abi3-py310` feature
and all `cp310-abi3` wheel selection and validation logic together. `abi3` does
not support free-threaded CPython.

## References

- [PyPA manylinux](https://github.com/pypa/manylinux)
- [PEP 600: manylinux](https://peps.python.org/pep-0600/)
- [PEP 656: musllinux](https://peps.python.org/pep-0656/)
- [Maturin documentation](https://www.maturin.rs/)

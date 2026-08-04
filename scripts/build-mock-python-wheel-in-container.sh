#!/bin/bash
set -e

# SANDBOX / TEST-ONLY build script.
# Builds a single abi3 (Python >= 3.9) wheel for the mssql-mock-tds-py crate
# inside a manylinux container. Unlike scripts/build-python-wheels-in-container.sh
# (mssql-py-core), there is NO per-Python-version loop: pyo3's abi3-py39 feature
# produces one forward-compatible wheel per (OS, arch).
#
# NOTE: the produced distribution is named "mssql-mock-tds" (pyproject [project].name
# + maturin module-name = mssql_mock_tds), but the Cargo package / folder is still
# mssql-mock-tds-py, so we always pass --manifest-path.

WORKSPACE_DIR="${WORKSPACE_DIR:-/workspace}"
OUTPUT_DIR="${OUTPUT_DIR:-$WORKSPACE_DIR/target/wheels}"

echo "==> Building mock TDS abi3 wheel in container"
echo "Workspace: $WORKSPACE_DIR"
echo "Output directory: $OUTPUT_DIR"

mkdir -p "$OUTPUT_DIR"

# Find a Python binary to drive maturin (abi3 only needs one interpreter).
FIRST_PYTHON=""
for py_path in /opt/python/cp312-cp312/bin/python /opt/python/cp3*/bin/python /usr/local/bin/python3 /usr/bin/python3; do
    if [ -x "$py_path" ]; then
        FIRST_PYTHON="$py_path"
        break
    fi
done

if [ -z "$FIRST_PYTHON" ]; then
    echo "Error: No Python installation found in container!"
    exit 1
fi

echo "Using Python: $FIRST_PYTHON"
$FIRST_PYTHON --version

# Rust + maturin are pre-installed in the *_rust container images.
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo &> /dev/null; then
    echo "Error: Rust not found! Ensure you're using a *_rust container image."
    exit 1
fi
rustc --version
cargo --version

if ! $FIRST_PYTHON -m pip show maturin &> /dev/null; then
    echo "Error: maturin not found! Ensure you're using a *_rust container image."
    exit 1
fi

# Optional cargo features (e.g. vendored-openssl for musllinux, where Alpine's
# dynamic libssl is not a portable runtime dependency). manylinux builds leave
# MATURIN_FEATURES unset and link the OS libssl.so.3 at runtime instead.
FEATURE_ARGS=()
if [ -n "${MATURIN_FEATURES:-}" ]; then
    echo "Building with cargo features: $MATURIN_FEATURES"
    FEATURE_ARGS=(--features "$MATURIN_FEATURES")
fi

# --auditwheel skip mirrors the mssql-py-core OpenSSL convention: do NOT let
# maturin repair the wheel. On manylinux the native extension links against the
# OS libssl.so.3 / libcrypto.so.3 at runtime (manylinux_2_34 / glibc 2.34); on
# musllinux we instead statically vendor OpenSSL via MATURIN_FEATURES so the
# wheel is self-contained. Passed on the CLI so we do not edit pyproject.toml.
$FIRST_PYTHON -m maturin build --release \
    --auditwheel skip \
    --interpreter "$FIRST_PYTHON" \
    "${FEATURE_ARGS[@]}" \
    --manifest-path "$WORKSPACE_DIR/mssql-mock-tds-py/Cargo.toml" \
    --out "$OUTPUT_DIR"

echo ""
echo "==> Wheel(s) built:"
ls -lh "$OUTPUT_DIR"

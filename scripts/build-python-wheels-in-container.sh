#!/bin/bash
set -e

# Build Python wheels inside manylinux/musllinux containers
# This script is designed to run inside the container

PYTHON_VERSIONS=("3.10" "3.11" "3.12" "3.13" "3.14")
WORKSPACE_DIR="${WORKSPACE_DIR:-/workspace}"
OUTPUT_DIR="${OUTPUT_DIR:-$WORKSPACE_DIR/target/wheels}"
# Platform tag the produced wheels should advertise. Must match the build
# container's actual ABI (e.g. manylinux_2_28 when building in a manylinux_2_28
# image, manylinux_2_34 in a manylinux_2_34 image, musllinux_1_2 in a
# musllinux_1_2 image). Passed to maturin as --compatibility so the wheel
# filename and WHEEL metadata carry the correct tag; otherwise maturin defaults
# to the un-publishable linux_* tag because auditwheel=skip is set in
# pyproject.toml (we deliberately disable library bundling).
MATURIN_COMPATIBILITY="${MATURIN_COMPATIBILITY:-}"

echo "==> Building Python wheels in container"
echo "Workspace: $WORKSPACE_DIR"
echo "Output directory: $OUTPUT_DIR"

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Find a Python binary for installing tools
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

# Verify Rust toolchain is available (pre-installed in _rust images)
echo ""
echo "==> Verifying Rust toolchain..."
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo &> /dev/null; then
    echo "❌ Error: Rust not found! Ensure you're using a *_rust container image."
    exit 1
fi
rustc --version
cargo --version

# Verify maturin is available (pre-installed in _rust images)
echo ""
echo "==> Verifying maturin..."
if ! $FIRST_PYTHON -m pip show maturin &> /dev/null; then
    echo "❌ Error: maturin not found! Ensure you're using a *_rust container image."
    exit 1
fi
echo "Maturin is available"

cd "$WORKSPACE_DIR/mssql-py-core"

# Wipe any prior build artifacts so this invocation links against THIS
# container's OpenSSL/glibc, not whatever a previous (different-flavor)
# invocation cached into target/. Each pipeline job runs the template once
# per manylinux flavor against the same workspace, so the cargo cache would
# otherwise carry openssl-sys objects from the previous flavor across runs.
echo ""
echo "==> Cleaning cargo target to avoid cross-flavor cache contamination"
cargo clean --manifest-path "$WORKSPACE_DIR/mssql-py-core/Cargo.toml" --quiet || true

# Build wheels for each Python version
for PY_VERSION in "${PYTHON_VERSIONS[@]}"; do
    # Find the Python binary (manylinux uses cpython naming)
    PYTHON_BIN=""
    
    # Try different naming conventions
    for py_path in /opt/python/cp${PY_VERSION//./}-*/bin/python /usr/local/bin/python${PY_VERSION} /usr/bin/python${PY_VERSION}; do
        if [ -x "$py_path" ]; then
            PYTHON_BIN="$py_path"
            break
        fi
    done
    
    if [ -z "$PYTHON_BIN" ]; then
        echo "⚠️  Python $PY_VERSION not found, skipping..."
        continue
    fi
    
    echo ""
    echo "==> Building wheel for Python $PY_VERSION using $PYTHON_BIN"
    $PYTHON_BIN --version

    MATURIN_ARGS=(build --release
        --interpreter "$PYTHON_BIN"
        --out "$OUTPUT_DIR"
        --manifest-path "$WORKSPACE_DIR/mssql-py-core/Cargo.toml")
    if [ -n "$MATURIN_COMPATIBILITY" ]; then
        echo "    Using --compatibility $MATURIN_COMPATIBILITY"
        MATURIN_ARGS+=(--compatibility "$MATURIN_COMPATIBILITY")
    fi

    $FIRST_PYTHON -m maturin "${MATURIN_ARGS[@]}"
    
    echo "✅ Wheel built successfully for Python $PY_VERSION"
done

# auditwheel=skip in pyproject.toml means maturin won't vendor shared libs
# (libssl, libcrypto) into the wheel. The native extension links against
# standard sonames and expects the OS to provide them at runtime.
# Run auditwheel show for diagnostic info only.
if command -v auditwheel &> /dev/null; then
    echo ""
    echo "==> Running auditwheel show (diagnostic only — bundling is disabled)..."
    for wheel in "$OUTPUT_DIR"/*.whl; do
        if [ -f "$wheel" ]; then
            echo "Checking: $(basename "$wheel")"
            auditwheel show "$wheel" || echo "⚠️  auditwheel check failed for $wheel"
        fi
    done
fi

echo ""
echo "==> All wheels built successfully!"
ls -lh "$OUTPUT_DIR"

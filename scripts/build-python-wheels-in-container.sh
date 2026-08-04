#!/bin/bash
set -e

# Build Python wheels inside manylinux/musllinux containers
# This script is designed to run inside the container

PYTHON_VERSIONS=("3.10" "3.11" "3.12" "3.13" "3.14")
WORKSPACE_DIR="${WORKSPACE_DIR:-/workspace}"
OUTPUT_DIR="${OUTPUT_DIR:-$WORKSPACE_DIR/target/wheels}"
# Optional: when set, a separate .debug file plus the stripped .so it belongs
# to are written to "$SYMBOLS_OUTPUT_DIR/<pytag>/" after each maturin build.
SYMBOLS_OUTPUT_DIR="${SYMBOLS_OUTPUT_DIR:-}"

# Split DWARF (.dwp) cannot be consumed once the skeleton units are stripped
# out of the shipped .so, so Linux links with debug info in the binary and the
# split happens below via objcopy. macOS/Windows keep the Cargo.toml default.
export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off

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
    
    $FIRST_PYTHON -m maturin build --release \
        --interpreter "$PYTHON_BIN" \
        --out "$OUTPUT_DIR" \
        --manifest-path "$WORKSPACE_DIR/mssql-py-core/Cargo.toml"
    
    echo "✅ Wheel built successfully for Python $PY_VERSION"

    # -----------------------------------------------------------------------
    # Split debug info out of the freshly built wheel.
    #
    # Operating on the .so *inside* the wheel guarantees the published debug
    # file belongs to exactly the binary we ship, and sidesteps the fact that
    # target/release/deps is overwritten by the next interpreter's build.
    # The helper rewrites the wheel entry-by-entry (preserving filename,
    # permissions, timestamps and compression) rather than repacking it.
    # -----------------------------------------------------------------------
    if [ -n "$SYMBOLS_OUTPUT_DIR" ]; then
        PY_TAG="cp${PY_VERSION//./}"
        SYM_DEST="$SYMBOLS_OUTPUT_DIR/$PY_TAG"
        mkdir -p "$SYM_DEST"

        if ! command -v objcopy &> /dev/null || ! command -v readelf &> /dev/null; then
            echo "❌ ERROR: binutils (objcopy/readelf) not found; cannot split debug info."
            exit 1
        fi

        WHEEL_PATH=$(find "$OUTPUT_DIR" -maxdepth 1 -name "*-${PY_TAG}-*.whl" | head -n1)
        if [ -z "$WHEEL_PATH" ]; then
            echo "❌ ERROR: no wheel matching *-${PY_TAG}-*.whl found in $OUTPUT_DIR"
            exit 1
        fi

        $FIRST_PYTHON "$WORKSPACE_DIR/scripts/split-wheel-debuginfo.py" "$WHEEL_PATH" "$SYM_DEST"
        ls -lh "$SYM_DEST"
    fi
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

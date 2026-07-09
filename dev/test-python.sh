#!/bin/bash
set -e

# Script to run Python tests for mssql-py-core
# This script sets up a Python virtual environment, installs dependencies, and runs pytest
# Usage: test-python.sh [--skip-integration] [--mssql-python] [--smoke] [--coverage]

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PY_CORE_DIR="$PROJECT_ROOT/mssql-py-core"
MOCK_TDS_PY_DIR="$PROJECT_ROOT/mssql-mock-tds-py"
MSSQL_PYTHON_DIR="${MSSQL_PYTHON_DIR:-$PROJECT_ROOT/../mssql-python}"
VENV_DIR="$PROJECT_ROOT/.venv-pycore"

# Parse command line arguments
SKIP_INTEGRATION=false
RUN_MSSQL_PYTHON=false
RUN_SMOKE_ONLY=false
COVERAGE=false
for arg in "$@"; do
    case $arg in
        --skip-integration) SKIP_INTEGRATION=true ;;
        --mssql-python) RUN_MSSQL_PYTHON=true ;;
        --smoke) RUN_SMOKE_ONLY=true ;;
        --coverage) COVERAGE=true ;;
    esac
done

# When --coverage is set, the mssql-py-core native extension is built with LLVM
# source-based instrumentation so the Rust code exercised by the Python tests is
# measured. Coverage output (Cobertura) is written to $COVERAGE_OUTPUT.
COVERAGE_OUTPUT="${COVERAGE_OUTPUT:-$PROJECT_ROOT/target/pycore-cobertura.xml}"

echo "==================================="
echo "Python Test Runner for mssql-py-core"
echo "==================================="

# Generate mock TDS server certificates if they don't exist
CERT_DIR="$PROJECT_ROOT/tests/test_certificates"
if [ ! -f "$CERT_DIR/valid_cert.pem" ] || [ ! -f "$CERT_DIR/key.pem" ]; then
    echo "Generating mock TDS server certificates..."
    bash "$PROJECT_ROOT/scripts/generate_mock_tds_server_certs.sh"
fi

# Check if Python is available
if ! command -v python3 &> /dev/null; then
    echo "Error: Python 3 is not installed"
    exit 1
fi

# Create virtual environment if it doesn't exist
if [ ! -d "$VENV_DIR" ]; then
    echo "Creating Python virtual environment at $VENV_DIR..."
    python3 -m venv "$VENV_DIR"
    echo "Virtual environment created."
fi

# Activate virtual environment
echo "Activating virtual environment..."
source "$VENV_DIR/bin/activate"

# Upgrade pip
echo "Upgrading pip..."
pip install --upgrade pip -q

# Install dependencies
echo "Installing dependencies (maturin, pytest, pytest-asyncio, python-dotenv, patchelf)..."
pip install maturin pytest pytest-asyncio python-dotenv patchelf -q

# Navigate to mssql-py-core directory
cd "$PY_CORE_DIR"

# Enable LLVM source-based coverage instrumentation for the native extension.
# `cargo llvm-cov show-env` exports RUSTFLAGS (-C instrument-coverage), a target
# dir and an LLVM_PROFILE_FILE pattern; maturin then builds an instrumented .so
# and the Python test process writes .profraw files as it runs.
if [ "$COVERAGE" = true ]; then
    echo "Enabling coverage instrumentation for mssql-py-core..."
    cargo llvm-cov clean --workspace
    # shellcheck disable=SC1090
    source <(cargo llvm-cov show-env --export-prefix)
fi

# Build and install the module in development mode
echo "Building mssql-py-core with maturin develop..."
maturin develop

# Build and install mssql-mock-tds-py (mock TDS server Python bindings)
echo "Building mssql-mock-tds-py with maturin develop..."
cd "$MOCK_TDS_PY_DIR"
PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 maturin develop

# Return to py-core directory for tests
cd "$PY_CORE_DIR"

# Run tests
echo ""
if [ "$RUN_SMOKE_ONLY" = true ]; then
    echo "Running pytest (smoke tests only)..."
    echo "==================================="
    pytest tests/smoke/ -v --tb=short --junitxml=pytest-smoke-results.xml
elif [ "$SKIP_INTEGRATION" = true ]; then
    echo "Running pytest (unit tests only, skipping integration and smoke tests)..."
    echo "==================================="
    pytest tests/ -v -m "not integration and not longhaul and not smoke" --ignore=tests/mssql_python --junitxml=pytest-results.xml
else
    echo "Running pytest (unit and integration tests, excluding longhaul and smoke tests)..."
    echo "==================================="
    pytest tests/ -v -m "not longhaul and not smoke" --ignore=tests/mssql_python --junitxml=pytest-results.xml
fi

echo ""
echo "==================================="
echo "Tests completed successfully!"
echo "==================================="

# Run mssql-python driver tests if requested
if [ "$RUN_MSSQL_PYTHON" = true ]; then
    echo ""
    echo "==================================="
    echo "Running mssql-python driver tests"
    echo "==================================="
    
    if [ ! -d "$MSSQL_PYTHON_DIR" ]; then
        echo "Error: mssql-python not found at $MSSQL_PYTHON_DIR"
        echo "Clone it as a sibling directory to mssql-tds"
        exit 1
    fi
    
    # Install additional dependencies for mssql-python mode
    echo "Installing mssql-python build dependencies (pybind11, wheel, setuptools)..."
    pip install pybind11 wheel setuptools -q
    
    # Install build dependencies on Linux (ODBC driver, cmake needed for ddbc_bindings)
    if [[ "$(uname -s)" == "Linux" ]]; then
        echo "Installing build dependencies (cmake, ODBC driver)..."
        apt-get update
        apt-get install -y cmake
        if ! dpkg -l 2>/dev/null | grep -q msodbcsql18; then
            curl -sSL -O https://packages.microsoft.com/config/ubuntu/22.04/packages-microsoft-prod.deb
            dpkg -i packages-microsoft-prod.deb || true
            rm packages-microsoft-prod.deb
            apt-get update
            ACCEPT_EULA=Y apt-get install -y msodbcsql18 unixodbc-dev
        fi
    fi
    
    # 1. Build ddbc_bindings (C++ pybind11) - needed for mssql-python import
    echo ""
    echo "Building ddbc_bindings..."
    cd "$MSSQL_PYTHON_DIR/mssql_python/pybind"
    chmod +x build.sh
    ./build.sh
    
    # 2. Build mssql-py-core wheel and extract .so
    echo ""
    echo "Building mssql-py-core wheel..."
    cd "$PY_CORE_DIR"
    rm -rf dist/
    maturin build --release -o dist/
    
    # Extract .so from wheel directly to mssql-python
    echo "Extracting mssql_py_core .so from wheel..."
    WHEEL_FILE=$(ls dist/*.whl | head -1)
    unzip -jo "$WHEEL_FILE" "mssql_py_core/*.so" -d "$MSSQL_PYTHON_DIR/mssql_python/"
    
    # Verify both .so files exist
    echo ""
    echo "Verifying bindings..."
    ls -la "$MSSQL_PYTHON_DIR/mssql_python/"*.so
    
    # 3. Install mssql-python and run tests
    echo ""
    echo "Installing mssql-python..."
    pip install -e "$MSSQL_PYTHON_DIR" -q
    
    # Run driver tests
    echo ""
    echo "Running tests/mssql_python/..."
    pytest "$PY_CORE_DIR/tests/mssql_python/" -v --junitxml="$PY_CORE_DIR/pytest-mssql-python-results.xml"
    
    echo ""
    echo "==================================="
    echo "mssql-python tests completed!"
    echo "==================================="
fi

# Generate the Cobertura coverage report for the instrumented native extension.
# Runs from the mssql-py-core directory so cargo-llvm-cov reads the profraw files
# produced while the Python tests exercised the compiled .so.
if [ "$COVERAGE" = true ]; then
    echo ""
    echo "==================================="
    echo "Generating mssql-py-core coverage report"
    echo "==================================="
    cd "$PY_CORE_DIR"
    mkdir -p "$(dirname "$COVERAGE_OUTPUT")"
    # Report generation is best-effort: the tests already ran and passed, so a
    # coverage tooling hiccup should not fail the test run.
    if cargo llvm-cov report --cobertura --output-path "$COVERAGE_OUTPUT"; then
        echo "Coverage report written to $COVERAGE_OUTPUT"
    else
        echo "WARNING: failed to generate mssql-py-core coverage report" >&2
    fi
fi

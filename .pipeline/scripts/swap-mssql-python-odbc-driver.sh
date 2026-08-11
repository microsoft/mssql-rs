#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Replace the Microsoft ODBC Driver 18 binary that mssql-python ships with the
# Rust mssql-odbc driver, so the upstream test suite exercises our driver.
#
# mssql-python resolves its driver entirely in native code
# (ddbc_bindings GetOdbcLibsBaseDir / GetDriverPathCpp): it takes the
# mssql_python_odbc package directory and appends
#   libs/<platform>/<arch>/lib/libmsodbcsql-<MAJOR>.<MINOR>.so.2.1
# where MAJOR.MINOR comes from mssql_python_odbc.__version__ at build time. So
# overwriting that one file is enough to redirect the whole suite - no
# mssql-python source change, no odbcinst registration, no LD_LIBRARY_PATH.
#
# Any copy of mssql_python_odbc installed into site-packages is swapped too, so
# the result does not depend on which one wins on sys.path.
#
# Env:
#   MSSQL_ODBC_DRIVER   Path to the built libmsodbcsql18.so (required).
#   MSSQL_PYTHON_DIR    mssql-python checkout (default: /workspace).

set -euo pipefail

MSSQL_PYTHON_DIR="${MSSQL_PYTHON_DIR:-/workspace}"
DRIVER_SO="${MSSQL_ODBC_DRIVER:-}"

if [ -z "$DRIVER_SO" ] || [ ! -f "$DRIVER_SO" ]; then
    echo "##[error]MSSQL_ODBC_DRIVER must point at the built libmsodbcsql18.so (got: '${DRIVER_SO:-unset}')"
    exit 1
fi

cd "$MSSQL_PYTHON_DIR"

VERSION_FILE="mssql_python_odbc/__init__.py"
if [ ! -f "$VERSION_FILE" ]; then
    echo "##[error]$MSSQL_PYTHON_DIR/$VERSION_FILE not found - is this an mssql-python checkout?"
    exit 1
fi

DRIVER_VERSION=$(awk -F= '/^__version__/ {gsub(/[[:space:]"]/, "", $2); print $2; exit}' "$VERSION_FILE")
DRIVER_MAJOR_MINOR=$(echo "$DRIVER_VERSION" | cut -d. -f1,2)
if [ -z "$DRIVER_MAJOR_MINOR" ]; then
    echo "##[error]Failed to parse mssql_python_odbc.__version__ from $VERSION_FILE"
    exit 1
fi
DRIVER_FILENAME="libmsodbcsql-${DRIVER_MAJOR_MINOR}.so.2.1"

echo "mssql_python_odbc.__version__ = $DRIVER_VERSION -> bundled driver file: $DRIVER_FILENAME"
echo "Replacement driver: $DRIVER_SO"
ls -la "$DRIVER_SO"

# Swap every copy of the bundled binary that this interpreter could resolve:
# the in-repo package (wins on sys.path when pytest runs from the repo root) and
# any site-packages copy pulled in as a dependency.
SEARCH_ROOTS=("$MSSQL_PYTHON_DIR")
while IFS= read -r site_dir; do
    [ -n "$site_dir" ] && [ -d "$site_dir" ] && SEARCH_ROOTS+=("$site_dir")
done < <(python -c "import site, sys; print('\n'.join(site.getsitepackages() + [site.getusersitepackages()]))" 2>/dev/null || true)

swapped=0
for root in "${SEARCH_ROOTS[@]}"; do
    while IFS= read -r target; do
        echo "Overwriting $target"
        cp -f "$DRIVER_SO" "$target"
        chmod 0755 "$target"
        swapped=$((swapped + 1))
    done < <(find "$root/mssql_python_odbc/libs" -type f -name "$DRIVER_FILENAME" 2>/dev/null || true)
done

if [ "$swapped" -eq 0 ]; then
    echo "##[error]No bundled $DRIVER_FILENAME found under: ${SEARCH_ROOTS[*]}"
    exit 1
fi

echo "Replaced $swapped bundled driver binaries with mssql-odbc"

# Surface unresolved shared-library dependencies here rather than as an opaque
# import failure inside the first test.
RESOLVED_TARGET="$MSSQL_PYTHON_DIR/mssql_python_odbc/libs/linux/debian_ubuntu/x86_64/lib/$DRIVER_FILENAME"
if [ -f "$RESOLVED_TARGET" ]; then
    echo "ldd $RESOLVED_TARGET"
    ldd "$RESOLVED_TARGET" || true
fi

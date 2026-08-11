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

DRIVER_VERSION=$(awk -F= '/^__version__/ {gsub(/[[:space:]"'"'"']/, "", $2); print $2; exit}' "$VERSION_FILE")
DRIVER_MAJOR_MINOR=$(echo "$DRIVER_VERSION" | cut -d. -f1,2)
if [ -z "$DRIVER_MAJOR_MINOR" ]; then
    echo "##[error]Failed to parse mssql_python_odbc.__version__ from $VERSION_FILE"
    exit 1
fi
DRIVER_FILENAME="libmsodbcsql-${DRIVER_MAJOR_MINOR}.so.2.1"

echo "mssql_python_odbc.__version__ = $DRIVER_VERSION -> bundled driver file: $DRIVER_FILENAME"
echo "Replacement driver: $DRIVER_SO"
ls -la "$DRIVER_SO"

# The native resolver keys off the __file__ of whichever mssql_python_odbc the
# interpreter actually imports, so ask Python instead of guessing which root wins
# on sys.path. Every reachable copy is still swapped below - this one just has to
# be among them.
PKG_DIR="$(python -c 'import os, mssql_python_odbc; print(os.path.dirname(os.path.abspath(mssql_python_odbc.__file__)))' 2>/dev/null || true)"
if [ -n "$PKG_DIR" ]; then
    echo "python resolves mssql_python_odbc to: $PKG_DIR"
else
    echo "##[warning]Could not import mssql_python_odbc to locate the package the tests will load"
fi

# Swap every copy of the bundled binary that this interpreter could resolve:
# the in-repo package (wins on sys.path when pytest runs from the repo root) and
# any site-packages copy pulled in as a dependency.
SEARCH_ROOTS=("$MSSQL_PYTHON_DIR")
[ -n "$PKG_DIR" ] && SEARCH_ROOTS+=("$(dirname "$PKG_DIR")")
while IFS= read -r site_dir; do
    [ -n "$site_dir" ] && [ -d "$site_dir" ] && SEARCH_ROOTS+=("$site_dir")
done < <(python -c "import site, sys; print('\n'.join(site.getsitepackages() + [site.getusersitepackages()]))" 2>/dev/null || true)

# Overlapping roots would otherwise swap - and count - the same file twice.
mapfile -t SEARCH_ROOTS < <(printf '%s\n' "${SEARCH_ROOTS[@]}" | awk 'NF && !seen[$0]++')

declare -a SWAPPED_TARGETS=()
for root in "${SEARCH_ROOTS[@]}"; do
    while IFS= read -r target; do
        echo "Overwriting $target"
        cp -f "$DRIVER_SO" "$target"
        chmod 0755 "$target"
        SWAPPED_TARGETS+=("$target")
    done < <(find "$root/mssql_python_odbc/libs" -type f -name "$DRIVER_FILENAME" 2>/dev/null || true)
done

if [ "${#SWAPPED_TARGETS[@]}" -eq 0 ]; then
    echo "##[error]No bundled $DRIVER_FILENAME found under: ${SEARCH_ROOTS[*]}"
    exit 1
fi

# Swapping some other copy while the importable package keeps the stock binary
# would silently run the whole suite against Microsoft's driver and report it as
# an mssql-odbc result.
if [ -n "$PKG_DIR" ]; then
    resolved_swapped=0
    for target in "${SWAPPED_TARGETS[@]}"; do
        case "$target" in "$PKG_DIR"/*) resolved_swapped=1 ;; esac
    done
    if [ "$resolved_swapped" -eq 0 ]; then
        echo "##[error]No swapped copy lives under the importable package dir ($PKG_DIR) - the suite would run against the stock driver"
        printf '  swapped: %s\n' "${SWAPPED_TARGETS[@]}"
        exit 1
    fi
fi

echo "Replaced ${#SWAPPED_TARGETS[@]} bundled driver binaries with mssql-odbc"

# Surface unresolved shared-library dependencies here rather than as an opaque
# import failure inside the first test. Every swapped copy is byte-identical and
# most sit in arch/libc slots this build could never be loaded from, so probe
# only the copy under the importable package - the one the resolver dlopens -
# and fall back to any swapped copy if the package dir could not be resolved.
probed=0
for target in "${SWAPPED_TARGETS[@]}"; do
    case "$target" in
        "${PKG_DIR:-__unresolved__}"/*)
            echo "ldd $target"
            ldd "$target" || true
            probed=1
            ;;
    esac
done

if [ "$probed" -eq 0 ]; then
    echo "ldd ${SWAPPED_TARGETS[0]}"
    ldd "${SWAPPED_TARGETS[0]}" || true
fi

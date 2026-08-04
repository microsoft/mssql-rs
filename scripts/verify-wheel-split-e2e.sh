#!/usr/bin/env bash
# End-to-end check of scripts/split-wheel-debuginfo.py against a real wheel:
# measures the shipped wheel size after the split and verifies the wheel is
# still installable and importable.
set -euo pipefail

PYTHON_BIN=/opt/python/cp312-cp312/bin/python
WHEEL=$(find /out/A -maxdepth 1 -name "*cp312*.whl" | head -1)
[ -n "$WHEEL" ] || { echo "no variant-A wheel found; run the comparison first"; exit 1; }

kb() { awk -v b="$(stat -c%s "$1")" 'BEGIN{printf "%.1f", b/1024}'; }

WORK=/out/E2E
rm -rf "$WORK"; mkdir -p "$WORK/sym"
cp "$WHEEL" "$WORK/"
WHEEL_COPY="$WORK/$(basename "$WHEEL")"

echo "before split: $(basename "$WHEEL_COPY")  $(kb "$WHEEL_COPY") KB"
BEFORE_NAME=$(basename "$WHEEL_COPY")

"$PYTHON_BIN" /workspace/scripts/split-wheel-debuginfo.py "$WHEEL_COPY" "$WORK/sym"

AFTER=$(find "$WORK" -maxdepth 1 -name "*.whl" | head -1)
echo "after  split: $(basename "$AFTER")  $(kb "$AFTER") KB"
[ "$(basename "$AFTER")" = "$BEFORE_NAME" ] && echo "PASS filename unchanged" || echo "FAIL filename changed"

echo "--- emitted symbol files ---"
find "$WORK/sym" -type f -printf '  %f  ' -exec sh -c 'awk -v b="$(stat -c%s "$1")" "BEGIN{printf \"%.1f KB\n\", b/1024}"' _ {} \;

echo "--- RECORD integrity + install + import ---"
"$PYTHON_BIN" -m pip install --quiet --force-reinstall "$AFTER"
"$PYTHON_BIN" -c "import mssql_py_core; print('PASS import ok')"

INSTALLED=$("$PYTHON_BIN" -c "
import mssql_py_core, pathlib
print(next(pathlib.Path(mssql_py_core.__file__).parent.glob('*.so')))")
echo "installed .so build-id: $(readelf -n "$INSTALLED" | awk '/Build ID/ {print $3}')"
echo "installed debug sects:  $(readelf -S "$INSTALLED" | grep -c '\.debug_')"
DBG=$(find "$WORK/sym" -name '*.debug' | head -1)
echo ".debug build-id:        $(readelf -n "$DBG" | awk '/Build ID/ {print $3}')"

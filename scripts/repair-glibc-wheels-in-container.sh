#!/bin/bash
# Repair glibc Linux wheels into manylinux wheels AFTER the ODBC driver has been
# injected, so auditwheel resolves and vendors the native dependencies of both
# the PyO3 extension and the embedded mssqlodbc.so. PyPI rejects bare linux_*
# tags; auditwheel retags the repaired wheels manylinux_*, which PyPI accepts.
# musllinux and non-linux wheels are left untouched. Run inside the matching
# manylinux build image (has auditwheel and the OpenSSL 3 runtime).
set -euo pipefail

WHEELS_DIR="${1:?usage: repair-glibc-wheels-in-container.sh <wheels-dir>}"
PLAT="${AUDITWHEEL_PLAT:-manylinux_2_34}"
AUDITWHEEL_BIN="${AUDITWHEEL_BIN:-auditwheel}"

if ! command -v "$AUDITWHEEL_BIN" >/dev/null 2>&1; then
    echo "ERROR: auditwheel not found in container" >&2
    exit 1
fi

shopt -s nullglob
repaired=0
for wheel in "$WHEELS_DIR"/*.whl; do
    name="$(basename "$wheel")"
    tag="${name%.whl}"
    tag="${tag##*-}"

    case "$tag" in
        linux_x86_64) plat="${PLAT}_x86_64" ;;
        linux_aarch64) plat="${PLAT}_aarch64" ;;
        *) continue ;;
    esac

    echo "==> Repairing $name -> $plat"
    tmp="$(mktemp -d)"
    "$AUDITWHEEL_BIN" repair --plat "$plat" --wheel-dir "$tmp" "$wheel"
    count=$(find "$tmp" -maxdepth 1 -name '*.whl' | wc -l)
    if [ "$count" -ne 1 ]; then
        echo "ERROR: expected exactly one repaired wheel for $name, got $count" >&2
        exit 1
    fi
    rm -f "$wheel"
    mv "$tmp"/*.whl "$WHEELS_DIR/"
    rm -rf "$tmp"
    repaired=$((repaired + 1))
done

if [ "$repaired" -eq 0 ]; then
    echo "ERROR: no bare glibc wheels found to repair in $WHEELS_DIR" >&2
    exit 1
fi

echo "Repaired $repaired glibc wheel(s) into $PLAT wheels."

#!/bin/bash
# Repair glibc Linux wheels into manylinux wheels AFTER the ODBC driver has been
# injected. PyPI rejects bare linux_* tags; auditwheel retags the repaired wheels
# manylinux_*, which PyPI accepts. libssl/libcrypto are excluded from the graft so
# the wheels keep using the OS-provided OpenSSL 3: native-tls resolves CA paths from
# the loaded libcrypto's compiled-in OPENSSLDIR, so vendoring AlmaLinux 9's OpenSSL
# would break server-certificate validation on Debian/Ubuntu. musllinux and
# non-linux wheels are left untouched. Run inside the matching manylinux build
# image (has auditwheel and the OpenSSL 3 runtime).
set -euo pipefail

WHEELS_DIR="${1:?usage: repair-glibc-wheels-in-container.sh <wheels-dir>}"
PLAT="${REPAIR_PLAT:-manylinux_2_34}"
AUDITWHEEL_BIN="${AUDITWHEEL_BIN:-auditwheel}"

# auditwheel reads AUDITWHEEL_PLAT as the default for --plat and validates it at
# parser-construction time, so a stray/arch-less value aborts before our explicit
# --plat is seen. We always pass --plat, so clear it.
unset AUDITWHEEL_PLAT

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
    "$AUDITWHEEL_BIN" repair --plat "$plat" \
        --exclude libssl.so.3 --exclude libcrypto.so.3 \
        --wheel-dir "$tmp" "$wheel"
    count=$(find "$tmp" -maxdepth 1 -name '*.whl' | wc -l)
    if [ "$count" -ne 1 ]; then
        echo "ERROR: expected exactly one repaired wheel for $name, got $count" >&2
        exit 1
    fi
    repaired_wheel="$(find "$tmp" -maxdepth 1 -name '*.whl')"
    repaired_name="$(basename "$repaired_wheel")"
    if [[ "$repaired_name" != *"-${plat}.whl" ]]; then
        echo "ERROR: auditwheel produced $repaired_name, expected platform tag $plat" >&2
        exit 1
    fi
    rm -f "$wheel"
    mv "$repaired_wheel" "$WHEELS_DIR/"
    rm -rf "$tmp"
    repaired=$((repaired + 1))
done

if [ "$repaired" -eq 0 ]; then
    echo "ERROR: no bare glibc wheels found to repair in $WHEELS_DIR" >&2
    exit 1
fi

echo "Repaired $repaired glibc wheel(s) into $PLAT wheels."

#!/usr/bin/env bash
# Compare debug-info strategies for the mssql-py-core wheel (Linux/manylinux).
#
# Variant A  current PR approach: link with embedded DWARF, then
#            objcopy --only-keep-debug / --strip-debug and rewrite the wheel.
# Variant B  split-DWARF: -Csplit-debuginfo=packed emits a .dwp next to a .so
#            that keeps only skeleton units. The wheel is never modified.
# Baseline   what main ships today: no [profile.release], so debug=0 and
#            strip defaults to "debuginfo".
#
# Run inside ghcr.io/microsoft/mssql-rs/python-build/manylinux_2_34_x86_64_rust.
set -euo pipefail

WORKSPACE_DIR="${WORKSPACE_DIR:-/workspace}"
RESULTS="${RESULTS:-/out/results.txt}"
PY_TAG=cp312
PYTHON_BIN=/opt/python/cp312-cp312/bin/python
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=/cargo-target
export CARGO_PROFILE_RELEASE_DEBUG=full
export CARGO_PROFILE_RELEASE_STRIP=none

mkdir -p "$(dirname "$RESULTS")"
: > "$RESULTS"

log() { echo "$*" | tee -a "$RESULTS"; }
kb() { awk -v b="$(stat -c%s "$1")" 'BEGIN{printf "%.1f", b/1024}'; }

# Report size, build-id and DWARF footprint of an ELF file.
describe() {
    local f="$1" label="$2"
    local bid dwarf
    bid=$(readelf -n "$f" 2>/dev/null | awk '/Build ID/ {print $3}')
    dwarf=$(readelf -S -W "$f" 2>/dev/null | sed 's/^ *\[[ 0-9]*\] *//' |
        awk '/^\.debug_/ {s+=strtonum("0x" $5)} END{printf "%.1f", s/1024}')
    log "$(printf '  %-34s %10s KB  debug-sections=%8s KB  build-id=%s' \
        "$label" "$(kb "$f")" "${dwarf:-0}" "${bid:0:16}")"
}

build() {
    local split="$1" outdir="$2"
    rm -rf "$outdir"; mkdir -p "$outdir"
    CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO="$split" \
        "$PYTHON_BIN" -m maturin build --release --interpreter "$PYTHON_BIN" \
        --out "$outdir" --manifest-path "$WORKSPACE_DIR/mssql-py-core/Cargo.toml" \
        >"$outdir/build.log" 2>&1 || { tail -40 "$outdir/build.log"; exit 1; }
}

extract_so() {
    local wheel="$1" dest="$2"
    rm -rf "$dest"; mkdir -p "$dest"
    "$PYTHON_BIN" -c "
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as z:
    for n in z.namelist():
        if n.endswith('.so'):
            open(sys.argv[2] + '/lib.so', 'wb').write(z.read(n)); break
" "$wheel" "$dest"
}

log "=== rustc: $(rustc --version) / maturin: $("$PYTHON_BIN" -m maturin --version) ==="
log ""

########################################################################
log "### Variant A - embedded DWARF + objcopy split (current PR)"
build off /out/A
WHEEL_A=$(find /out/A -name "*${PY_TAG}*.whl" | head -1)
extract_so "$WHEEL_A" /out/A/so
log "  wheel as built by maturin:          $(kb "$WHEEL_A") KB"
describe /out/A/so/lib.so "so in wheel (unsplit)"

cp /out/A/so/lib.so /out/A/so/split.so
objcopy --only-keep-debug /out/A/so/split.so /out/A/so/lib.so.debug
objcopy --strip-debug /out/A/so/split.so
objcopy --add-gnu-debuglink=/out/A/so/lib.so.debug /out/A/so/split.so
describe /out/A/so/split.so "-> shipped .so (stripped)"
describe /out/A/so/lib.so.debug "-> published .debug"
log ""

########################################################################
log "### Variant B - split-DWARF (.dwp), wheel untouched"
build packed /out/B
WHEEL_B=$(find /out/B -name "*${PY_TAG}*.whl" | head -1)
extract_so "$WHEEL_B" /out/B/so
log "  wheel as built by maturin:          $(kb "$WHEEL_B") KB"
describe /out/B/so/lib.so "-> shipped .so (skeleton only)"
DWP=$(find "$CARGO_TARGET_DIR/release" -maxdepth 1 -name "*.dwp" | head -1)
if [ -n "$DWP" ]; then
    log "$(printf '  %-34s %10s KB' '-> published .dwp' "$(kb "$DWP")")"
    log "  .dwp build-id: $(readelf -n "$DWP" 2>/dev/null | awk '/Build ID/ {print $3}' || echo '(none)')"
else
    log "  !! no .dwp produced"
fi
log ""

########################################################################
log "### Baseline - what main ships today (debug=0, strip=debuginfo)"
CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=debuginfo build off /out/C
WHEEL_C=$(find /out/C -name "*${PY_TAG}*.whl" | head -1)
extract_so "$WHEEL_C" /out/C/so
log "  wheel as built by maturin:          $(kb "$WHEEL_C") KB"
describe /out/C/so/lib.so "-> shipped .so"
log ""
log "=== done ==="

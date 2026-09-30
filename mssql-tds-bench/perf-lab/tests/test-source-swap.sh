#!/usr/bin/env bash
# Regression tests for the perf-lab harness's baseline source-swap helpers:
# version stamping, idempotent restore, and cleanup ordering.
#
# Run: mssql-tds-bench/perf-lab/tests/test-source-swap.sh
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/../run-benchmarks.sh"
FAILED=0

pass()  { echo "  ok   - $1"; }
fail()  { echo "  FAIL - $1" >&2; FAILED=1; }
check() { if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (expected [$3], got [$2])"; fi; }

# Load the helpers straight out of the harness so these tests exercise the real
# implementations and cannot drift from them.
eval "$(awk '/^(package_version|set_package_version|align_baseline_version|swap_to_baseline|restore_candidate)\(\)/,/^}/' "$SCRIPT")"
for fn in package_version set_package_version align_baseline_version swap_to_baseline restore_candidate; do
    declare -F "$fn" >/dev/null || { echo "FAIL - could not extract $fn from $SCRIPT" >&2; exit 1; }
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

write_manifest() { # $1 = crate dir, $2 = package version
    mkdir -p "$1"
    cat > "$1/Cargo.toml" <<EOF
[package]
name = "mssql-tds"
version = "$2"
edition = "2024"

[dependencies]
tokio = { version = "1.0" }
EOF
}

new_repo() { # $1 = repo root, $2 = candidate version, $3 = baseline version
    rm -rf "$1"
    write_manifest "$1/mssql-tds" "$2"
    echo candidate > "$1/mssql-tds/MARKER"
    write_manifest "$1/tree/mssql-tds" "$3"
    echo baseline > "$1/tree/mssql-tds/MARKER"
}

echo "manifest helpers"
write_manifest "$WORK/m" 0.1.0
check "package_version reads the [package] version" "$(package_version "$WORK/m/Cargo.toml")" "0.1.0"
set_package_version "$WORK/m/Cargo.toml" "9.9.9"
check "set_package_version rewrites the package version" "$(package_version "$WORK/m/Cargo.toml")" "9.9.9"
check "set_package_version leaves dependency versions alone" "$(grep -c 'version = "1.0"' "$WORK/m/Cargo.toml")" "1"
check "set_package_version preserves the line count" "$(wc -l < "$WORK/m/Cargo.toml")" "7"

echo "swap and restore"
REPO_ROOT="$WORK/r1"; BASELINE_TREE="$WORK/r1/tree"
new_repo "$REPO_ROOT" 0.2.0 0.1.0
swap_to_baseline >/dev/null
check "swap installs the baseline source" "$(cat "$REPO_ROOT/mssql-tds/MARKER")" "baseline"
check "swap stamps the candidate version onto the baseline" "$(package_version "$REPO_ROOT/mssql-tds/Cargo.toml")" "0.2.0"
restore_candidate
check "restore brings the candidate back" "$(cat "$REPO_ROOT/mssql-tds/MARKER")" "candidate"
restore_candidate
check "restore is idempotent (second call is a no-op)" "$(cat "$REPO_ROOT/mssql-tds/MARKER")" "candidate"

echo "matching versions"
REPO_ROOT="$WORK/r2"; BASELINE_TREE="$WORK/r2/tree"
new_repo "$REPO_ROOT" 0.2.0 0.2.0
check "no stamping message when versions already match" "$(swap_to_baseline | grep -c Stamping)" "0"
restore_candidate

echo "unreadable baseline version"
REPO_ROOT="$WORK/r3"; BASELINE_TREE="$WORK/r3/tree"
new_repo "$REPO_ROOT" 0.2.0 0.1.0
grep -v '^version' "$BASELINE_TREE/mssql-tds/Cargo.toml" > "$WORK/stripped"
mv "$WORK/stripped" "$BASELINE_TREE/mssql-tds/Cargo.toml"
( swap_to_baseline >/dev/null 2>&1 )
check "swap fails loudly when the baseline version is unreadable" "$?" "1"
restore_candidate
check "candidate is recoverable after a failed swap" "$(cat "$REPO_ROOT/mssql-tds/MARKER")" "candidate"

# The swap is fallible, so the cleanup handler must already be armed when it runs;
# otherwise a stamping failure strands the candidate in .mssql-tds-candidate.
echo "cleanup ordering"
trap_line="$(grep -n 'restore_candidate 2>/dev/null' "$SCRIPT" | grep EXIT | head -1 | cut -d: -f1)"
swap_line="$(grep -n '^swap_to_baseline$' "$SCRIPT" | head -1 | cut -d: -f1)"
if [ -n "$trap_line" ] && [ -n "$swap_line" ] && [ "$trap_line" -lt "$swap_line" ]; then
    pass "EXIT trap is armed before the source swap"
else
    fail "EXIT trap must be armed before swap_to_baseline (trap@${trap_line:-none}, swap@${swap_line:-none})"
fi

[ "$FAILED" -eq 0 ] && echo "All source-swap tests passed." || echo "Source-swap tests FAILED." >&2
exit "$FAILED"

#!/bin/bash
# SANDBOX / TEST-ONLY helper.
# Stamps a PEP 440 version into BOTH mssql-mock-tds-py/pyproject.toml and
# mssql-mock-tds-py/Cargo.toml so maturin bakes the same version into the wheel.
# Used by every non-Windows build job (manylinux, musllinux, macOS).
#
# Env:
#   RELEASE_VERSION  "True" => publish the base version as-is (e.g. 1.0.0).
#                    Anything else => append a .dev<date><buildId> segment.
#   BUILD_BUILDID    Azure DevOps build id, used in the dev segment.
#
# Emits the resolved version as the `mockWheelVersion` pipeline variable.
set -euo pipefail

PYPROJECT="mssql-mock-tds-py/pyproject.toml"
CARGO="mssql-mock-tds-py/Cargo.toml"

BASE=$(grep -m1 -E '^version\s*=' "$PYPROJECT" | sed -E 's/.*"([^"]+)".*/\1/')

case "${RELEASE_VERSION:-}" in
  True|true|TRUE) VER="${BASE}" ;;                                    # release: publish BASE as-is
  *) VER="${BASE}.dev$(date -u +%Y%m%d)${BUILD_BUILDID:-}" ;;         # PEP 440 dev release segment (.devN)
esac

echo "Sandbox wheel version: ${VER}"

# GNU sed (Linux) and BSD sed (macOS) disagree on the in-place flag. Each manifest
# has exactly one line-starting `version =` (the package version), so a plain
# substitution is unambiguous.
sed_inplace() {
  local pattern="$1" file="$2"
  if sed --version >/dev/null 2>&1; then
    sed -i -E "$pattern" "$file"
  else
    sed -i '' -E "$pattern" "$file"
  fi
}

sed_inplace "s/^version[[:space:]]*=.*/version = \"${VER}\"/" "$PYPROJECT"
sed_inplace "s/^version[[:space:]]*=.*/version = \"${VER}\"/" "$CARGO"

echo "##vso[task.setvariable variable=mockWheelVersion]${VER}"

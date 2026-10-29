#!/bin/bash
# SANDBOX / TEST-ONLY helper.
# Stamps the run's version into BOTH mssql-mock-tds-py/pyproject.toml and
# mssql-mock-tds-py/Cargo.toml. maturin reads the wheel version from
# pyproject.toml's [project].version, so pyproject gets the PEP 440 spelling
# (0.1.0.dev123) while Cargo.toml gets the SemVer one (0.1.0-dev.123) that
# `cargo metadata` will accept.
# Used by every non-Windows build job (manylinux, musllinux, macOS).
#
# Env:
#   WHEEL_VERSION    Precomputed version from the run's single compute step. When
#                    set, it is stamped verbatim (all jobs share one version). When
#                    empty, the version is computed here as a fallback.
#   RELEASE_VERSION  "True" => publish the base version as-is (e.g. 1.0.0).
#                    Anything else => append a .dev<date><buildId> segment.
#   BUILD_BUILDID    Azure DevOps build id, used in the dev segment.
#
# Emits the resolved version as the `mockWheelVersion` pipeline variable.
set -euo pipefail

PYPROJECT="mssql-mock-tds-py/pyproject.toml"
CARGO="mssql-mock-tds-py/Cargo.toml"

if [ -n "${WHEEL_VERSION:-}" ]; then
  VER="${WHEEL_VERSION}"                                               # shared, computed once upstream
else
  BASE=$(grep -m1 -E '^version\s*=' "$PYPROJECT" | sed -E 's/.*"([^"]+)".*/\1/')
  case "${RELEASE_VERSION:-}" in
    True|true|TRUE) VER="${BASE}" ;;                                   # release: publish BASE as-is
    *) VER="${BASE}.dev$(date -u +%Y%m%d)${BUILD_BUILDID:-}" ;;        # PEP 440 dev release segment (.devN)
  esac
fi

# Cargo rejects PEP 440's `.devN` suffix ("unexpected character '.' after patch
# version number") because SemVer spells a prerelease with a hyphen.
if [[ "${VER}" =~ ^(.*)\.dev([0-9]+)$ ]]; then
  CARGO_VER="${BASH_REMATCH[1]}-dev.${BASH_REMATCH[2]}"
else
  CARGO_VER="${VER}"                                                   # release versions are already valid SemVer
fi

echo "Sandbox wheel version: ${VER} (Cargo manifest: ${CARGO_VER})"

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
sed_inplace "s/^version[[:space:]]*=.*/version = \"${CARGO_VER}\"/" "$CARGO"

echo "##vso[task.setvariable variable=mockWheelVersion]${VER}"

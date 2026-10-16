#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Stamps the run's version into the mssql-py-core wheel manifests (non-Windows
# jobs: manylinux, musllinux, macOS). maturin reads the wheel version from
# pyproject.toml's [project].version, so pyproject gets the PEP 440 spelling
# (0.1.0.dev123) while Cargo.toml gets the SemVer one (0.1.0-dev.123) that
# `cargo metadata` accepts. Runs on the host before the container build so the
# mounted source carries the stamped version into maturin.
#
# Env:
#   WHEEL_VERSION  Version resolved once by compute-python-wheel-version.ps1.
#                  Empty is a no-op (base version used unchanged).
#   PYPROJECT      pyproject.toml path (default mssql-py-core/pyproject.toml).
#   CARGO          Cargo.toml path (default mssql-py-core/Cargo.toml).
set -euo pipefail

PYPROJECT="${PYPROJECT:-mssql-py-core/pyproject.toml}"
CARGO="${CARGO:-mssql-py-core/Cargo.toml}"

if [ -z "${WHEEL_VERSION:-}" ]; then
  echo "No version supplied; leaving manifests at their base version."
  exit 0
fi

VER="${WHEEL_VERSION}"

# Cargo rejects PEP 440's `.devN` suffix ("unexpected character '.' after patch
# version number") because SemVer spells a prerelease with a hyphen.
if [[ "${VER}" =~ ^(.*)\.dev([0-9]+)$ ]]; then
  CARGO_VER="${BASH_REMATCH[1]}-dev.${BASH_REMATCH[2]}"
else
  CARGO_VER="${VER}"
fi

echo "Wheel version: ${VER} (Cargo manifest: ${CARGO_VER})"

# GNU sed (Linux) and BSD sed (macOS) disagree on the in-place flag. Each manifest
# has exactly one line-starting `version =` (the package version).
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

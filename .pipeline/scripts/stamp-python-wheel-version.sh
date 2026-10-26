#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Stamps the run's wheel version into [project].version of mssql-py-core's
# pyproject.toml (non-Windows jobs: manylinux, musllinux, macOS). Runs on the host
# before the container build so the mounted source carries the stamped version
# into maturin. Twin of stamp-python-wheel-version.ps1.
#
# maturin takes the wheel version from the static [project].version, so that is
# the only field stamped; Cargo.toml's independent crate version is left alone.
# Only the `version` line inside the [project] table is rewritten. Fails if that
# table has no single `version = "..."` line, or if the value is not a PEP 440
# version (for example an unexpanded `$(pyCoreWheelVersion)` macro).
#
# Env:
#   WHEEL_VERSION  Version resolved once by compute-python-wheel-version.ps1.
#   PYPROJECT      pyproject.toml path (default mssql-py-core/pyproject.toml).
set -euo pipefail

PYPROJECT="${PYPROJECT:-mssql-py-core/pyproject.toml}"
VER="${WHEEL_VERSION:-}"

if [[ ! "${VER}" =~ ^[0-9]+(\.[0-9]+)*((a|b|rc)[0-9]+)?(\.post[0-9]+)?(\.dev[0-9]+)?$ ]]; then
  echo "Refusing to stamp '${VER}' into ${PYPROJECT}: not a PEP 440 version." \
       "An unexpanded \$(...) macro means the compute step emitted no value." >&2
  exit 1
fi

TMP="$(mktemp)"
trap 'rm -f "${TMP}"' EXIT

# Portable across GNU awk/mawk (Linux) and BSD awk (macOS). A table header resets
# the scope, so [project.urls] and later tables are never touched.
if ! awk -v ver="${VER}" '
  /^\[/ { in_project = ($0 ~ /^\[project\][ \t\r]*$/) }
  in_project && /^[ \t]*version[ \t]*=[ \t]*"[^"]*"/ {
    count++
    sub(/"[^"]*"/, "\"" ver "\"")
  }
  { print }
  END { if (count != 1) exit 3 }
' "${PYPROJECT}" > "${TMP}"; then
  echo "Expected one version line in [project] of ${PYPROJECT}" >&2
  exit 1
fi

cat "${TMP}" > "${PYPROJECT}"
echo "Stamped [project].version = \"${VER}\" in ${PYPROJECT}"

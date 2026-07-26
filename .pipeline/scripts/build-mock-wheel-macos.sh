#!/bin/bash
# SANDBOX / TEST-ONLY helper. Builds ONE macOS universal2 abi3 wheel for the mock
# TDS crate. vendored-openssl statically links OpenSSL so the wheel does not depend
# on Homebrew's libssl. --manifest-path is required because the Cargo package is
# still mssql-mock-tds-py.
#
# Env:
#   OB_OUTPUTDIRECTORY  Job artifact staging dir; the wheel is written to
#                       $OB_OUTPUTDIRECTORY/wheels. Required.
set -euo pipefail

: "${OB_OUTPUTDIRECTORY:?OB_OUTPUTDIRECTORY is required}"
# Target the oldest macOS we support so the wheel installs broadly. The retag step
# stamps a matching macosx_11_0_universal2 platform tag.
export MACOSX_DEPLOYMENT_TARGET=11.0

WHEELS_DIR="${OB_OUTPUTDIRECTORY}/wheels"
mkdir -p "$WHEELS_DIR"

maturin build --release \
  --target universal2-apple-darwin \
  --auditwheel skip \
  --features vendored-openssl \
  --manifest-path mssql-mock-tds-py/Cargo.toml \
  --out "$WHEELS_DIR"

ls -lh "$WHEELS_DIR"

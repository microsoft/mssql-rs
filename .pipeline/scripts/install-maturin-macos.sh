#!/bin/bash
# SANDBOX / TEST-ONLY helper. Installs maturin and the Rust targets needed for a
# macOS universal2 build.
#
# The UsePythonVersion-selected interpreter is a managed, writable, on-PATH Python,
# so there is no PEP 668 externally-managed-environment problem (no
# --break-system-packages) and the maturin/wheel console scripts land on PATH.
# abi3 needs only one interpreter regardless of the wheel's >=3.9 compatibility.
set -euo pipefail

python -m pip install --upgrade pip
python -m pip install maturin wheel

echo "Installing Rust targets for universal2 build..."
rustup target add x86_64-apple-darwin
rustup target add aarch64-apple-darwin

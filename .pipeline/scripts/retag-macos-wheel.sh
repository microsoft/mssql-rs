#!/bin/bash
# SANDBOX / TEST-ONLY helper. Re-tags the freshly built macOS wheel(s) to a uniform
# universal2 platform tag. `wheel` was installed against the selected interpreter,
# so `-m wheel` avoids relying on the console script's location on PATH.
#
# Env:
#   OB_OUTPUTDIRECTORY  Job artifact staging dir containing wheels/. Required.
set -euo pipefail

: "${OB_OUTPUTDIRECTORY:?OB_OUTPUTDIRECTORY is required}"
WHEELS_DIR="${OB_OUTPUTDIRECTORY}/wheels"

for whl in "$WHEELS_DIR"/*.whl; do
  echo "Re-tagging: $(basename "$whl")"
  python -m wheel tags --platform-tag macosx_15_0_universal2 --remove "$whl"
done

echo "Re-tagged wheels:"
ls -lh "$WHEELS_DIR"

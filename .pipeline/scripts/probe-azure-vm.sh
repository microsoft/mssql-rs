#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

printf 'Azure VM probe: uid=%s; architecture=%s\n' "$(id -u)" "$(uname -m)"
tag="$(cat /sys/class/dmi/id/chassis_asset_tag)"
expected='7783-7084-3265-9085-8269-3286-77'
if [[ "$tag" != "$expected" ]]; then
    echo 'Azure VM probe: azurePublicCloudTagMatch=false' >&2
    exit 1
fi
printf 'Azure VM probe: azurePublicCloudTagMatch=true; SMBIOSAssetTag=%s\n' "$expected"

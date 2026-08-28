#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Installs and verifies one exact Microsoft ODBC Driver 18 package on Ubuntu.

set -euo pipefail

VERSION="${1:-18.6.2.1}"
if ! [[ "$VERSION" =~ ^[0-9]+(\.[0-9]+){3}$ ]]; then
    echo "ERROR: invalid msodbcsql version: $VERSION" >&2
    exit 1
fi
PACKAGE_VERSION="${VERSION}-1"

if [ ! -r /etc/os-release ]; then
    echo "ERROR: /etc/os-release is unavailable" >&2
    exit 1
fi
# shellcheck disable=SC1091
source /etc/os-release
if [ "${ID:-}" != "ubuntu" ] || [ -z "${VERSION_ID:-}" ]; then
    echo "ERROR: the pinned msodbcsql installer requires Ubuntu" >&2
    exit 1
fi

sudo_command=()
if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 ||
        { echo "ERROR: sudo is required to install msodbcsql" >&2; exit 1; }
    sudo_command=(sudo)
fi

temp_dir="$(mktemp -d)"
cleanup() {
    rm -rf "$temp_dir"
}
trap cleanup EXIT

echo ">>> Installing Microsoft ODBC Driver $VERSION..."
"${sudo_command[@]}" apt-get update -y
"${sudo_command[@]}" env DEBIAN_FRONTEND=noninteractive \
    apt-get install -y --no-install-recommends curl ca-certificates

curl -fsSL https://packages.microsoft.com/keys/microsoft.asc \
    -o "$temp_dir/microsoft.asc"
curl -fsSL "https://packages.microsoft.com/config/ubuntu/$VERSION_ID/prod.list" \
    -o "$temp_dir/mssql-release.list"
"${sudo_command[@]}" install -d -m 0755 /usr/share/keyrings
"${sudo_command[@]}" install -m 0644 "$temp_dir/microsoft.asc" \
    /usr/share/keyrings/microsoft.asc
if ! grep -q 'signed-by=' "$temp_dir/mssql-release.list"; then
    sed -i 's#^deb \[#deb [signed-by=/usr/share/keyrings/microsoft.asc #' \
        "$temp_dir/mssql-release.list"
fi
grep -q 'signed-by=/usr/share/keyrings/microsoft.asc' \
    "$temp_dir/mssql-release.list" ||
    { echo "ERROR: failed to scope the Microsoft apt key" >&2; exit 1; }
"${sudo_command[@]}" install -m 0644 "$temp_dir/mssql-release.list" \
    /etc/apt/sources.list.d/mssql-release.list

"${sudo_command[@]}" apt-get update -y
"${sudo_command[@]}" env DEBIAN_FRONTEND=noninteractive ACCEPT_EULA=Y \
    apt-get install -y --no-install-recommends --allow-downgrades \
    "msodbcsql18=$PACKAGE_VERSION"

installed_version="$(dpkg-query -W -f='${Version}' msodbcsql18)"
if [ "$installed_version" != "$PACKAGE_VERSION" ]; then
    echo "ERROR: installed msodbcsql18 $installed_version; expected $PACKAGE_VERSION" >&2
    exit 1
fi
echo ">>> Installed Microsoft ODBC Driver $VERSION ($installed_version)."

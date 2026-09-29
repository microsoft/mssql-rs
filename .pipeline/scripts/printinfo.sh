#!/bin/bash

# Azure Linux 3 agents. This used to carry hardcoded Ubuntu USN numbers and
# fixed-version strings for packages we were waiting on; Azure Linux tracks its
# own advisory stream, so it now just reports what is installed.

echo "PATH variable:"
echo "$PATH"

echo ""
echo "Host:"
if [ -r /etc/os-release ]; then
    . /etc/os-release
    echo "  ${PRETTY_NAME:-unknown} ($(uname -m), kernel $(uname -r))"
fi

echo ""
echo "Azure CLI information:"
if command -v az &> /dev/null; then
    az version
else
    echo "Azure CLI is not installed"
fi

echo ""
echo "Package versions:"
for pkg in gss-ntlmssp binutils libssh libxml2 git-lfs python3-pip openssl \
           openssl-devel krb5-devel; do
    if rpm -q "$pkg" >/dev/null 2>&1; then
        printf '  %-16s %s\n' "$pkg" "$(rpm -q --qf '%{VERSION}-%{RELEASE}' "$pkg")"
    else
        printf '  %-16s not installed\n' "$pkg"
    fi
done

echo ""
echo "Pending updates:"
tdnf check-update 2>&1 | tail -20

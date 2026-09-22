#!/bin/bash

# The advisory text below is Ubuntu-specific. Azure Linux 3 tracks its own
# stream, so on rpm hosts we report installed versions and skip the USN links.
if command -v dpkg >/dev/null 2>&1; then
    PKG_QUERY="dpkg"
elif command -v rpm >/dev/null 2>&1; then
    PKG_QUERY="rpm"
else
    PKG_QUERY="none"
fi

pkg_version() {
    case "$PKG_QUERY" in
        dpkg) dpkg-query -W -f='${Version}' "$1" 2>/dev/null ;;
        rpm)  rpm -q --qf '%{VERSION}-%{RELEASE}' "$1" 2>/dev/null ;;
    esac
}

pkg_installed() {
    case "$PKG_QUERY" in
        dpkg) dpkg-query -W -f='${db:Status-Status}' "$1" 2>/dev/null | grep -q '^installed$' ;;
        rpm)  rpm -q "$1" >/dev/null 2>&1 ;;
        *)    return 1 ;;
    esac
}

echo "PATH variable:"
echo "$PATH"
echo ""
echo "Package database: $PKG_QUERY"

echo ""
echo "Azure CLI information:"
if command -v az &> /dev/null; then
    az version
else
    echo "Azure CLI is not installed"
fi

echo ""
echo "gss-ntlmssp package information:"
if pkg_installed gss-ntlmssp; then
    echo "gss-ntlmssp: $(pkg_version gss-ntlmssp)"
    if [ "$PKG_QUERY" = "dpkg" ]; then
        echo "Check vulnerability: https://ubuntu.com/security/notices/USN-7588-1"
        echo "Vulnerable version: 0.7.0-4build4"
        echo "Fixed version: 0.7.0-4ubuntu0.22.04.1~esm1"
    fi
else
    echo "gss-ntlmssp package is not installed"
fi

echo ""
echo "binutils packages information:"
for pkg in libbinutils binutils-common binutils-aarch64-linux-gnu libctf0 binutils libctf-nobfd0; do
    if pkg_installed "$pkg"; then
        echo "$pkg: $(pkg_version "$pkg")"
    fi
done
if [ "$PKG_QUERY" = "dpkg" ]; then
    echo "Vulnerable version: 2.38-4ubuntu2.8"
    echo "Fixed version: 2.38-4ubuntu2.10"
fi

echo ""
echo "libssh package information:"
for pkg in libssh-4 libssh; do
    if pkg_installed "$pkg"; then
        echo "$pkg: $(pkg_version "$pkg")"
        if [ "$PKG_QUERY" = "dpkg" ]; then
            echo "Vulnerable version: 0.9.6-2ubuntu0.22.04.4"
            echo "Fixed version: 0.9.6-2ubuntu0.22.04.5"
        fi
        found_libssh=1
    fi
done
[ -z "${found_libssh:-}" ] && echo "libssh package is not installed"

echo ""
echo "libxml2 package information:"
if pkg_installed libxml2; then
    echo "libxml2: $(pkg_version libxml2)"
    if [ "$PKG_QUERY" = "dpkg" ]; then
        echo "Vulnerable version: 2.9.13+dfsg-1ubuntu0.9"
        echo "Fixed version: 2.9.13+dfsg-1ubuntu0.10"
    fi
else
    echo "libxml2 package is not installed"
fi


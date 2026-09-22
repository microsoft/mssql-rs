#!/bin/bash

export DEBIAN_FRONTEND=noninteractive

print_info() {
    arch
    echo "Current dir is $(pwd)"
    ip addr
}

print_info

# Parse optional arch argument
ARCH=$(arch)

if [ "$ARCH" != "x86_64" ] && [ "$ARCH" != "aarch64" ]; then
    echo "Unknown arch: $ARCH"
    exit 1
fi

# Azure Linux 3 agents are replacing the Ubuntu ones, whose git-lfs,
# gss-ntlmssp and python3-pip CVEs are fixed on jammy only under Ubuntu Pro. So
# resolve the package manager rather than assuming apt.
if [ -r /etc/os-release ]; then
    . /etc/os-release
fi

if command -v apt-get >/dev/null 2>&1; then
    PKG_MGR="apt"
elif command -v tdnf >/dev/null 2>&1; then
    PKG_MGR="tdnf"
elif command -v dnf >/dev/null 2>&1; then
    PKG_MGR="dnf"
else
    echo "ERROR: no supported package manager found (looked for apt-get, tdnf, dnf)"
    echo "       ID=${ID:-unknown} VERSION_ID=${VERSION_ID:-unknown}"
    exit 1
fi
echo "INFO: package manager is $PKG_MGR (ID=${ID:-unknown} VERSION_ID=${VERSION_ID:-unknown})"

if [ "$PKG_MGR" = "apt" ]; then
    DEPS="jq \
        unzip \
        build-essential \
        pkg-config \
        libssl-dev \
        libkrb5-dev \
        python-is-python3 \
        python3.10-venv \
        pip \
        python3-pip \
        wget \
        apt-transport-https \
        software-properties-common"
    DOCKER_PKG="docker.io"
else
    # apt-transport-https and software-properties-common are apt plumbing with no
    # rpm equivalent; python-is-python3 is replaced by the symlink below.
    DEPS="jq \
        unzip \
        build-essential \
        pkg-config \
        openssl-devel \
        krb5-devel \
        python3 \
        python3-pip \
        python3-devel \
        wget \
        ca-certificates"
    DOCKER_PKG="moby-engine"
fi

# Docker is baked into the x64 images; only ARM has ever installed it here.
if [ "$ARCH" = "aarch64" ]; then
    DEPS="$DEPS $DOCKER_PKG"
fi

pkg_update() {
    case "$PKG_MGR" in
        apt)  sudo apt update ;;
        tdnf) sudo tdnf -y makecache ;;
        dnf)  sudo dnf -y makecache ;;
    esac
}

pkg_install() {
    case "$PKG_MGR" in
        apt)  sudo apt install "$@" -y ;;
        tdnf) sudo tdnf install "$@" -y ;;
        dnf)  sudo dnf install "$@" -y ;;
    esac
}

update_ok=false
for i in {1..5}; do
    pkg_update && { update_ok=true; break; }
    echo "$PKG_MGR update failed, retrying in 5 seconds... (attempt $i/5)"
    sleep $((30 * i))
done
if [ "$update_ok" != true ]; then
    echo "ERROR: $PKG_MGR update failed after 5 attempts"
    exit 1
fi

# Needed for msrustup download and essentials for building rust binaries.
# Try installing dependencies up to 5 times if it fails
install_ok=false
for i in {1..5}; do
    pkg_install $DEPS && { install_ok=true; break; }
    echo "$PKG_MGR install failed, retrying in 5 seconds... (attempt $i/5)"
    sleep $((30 * i))
done
if [ "$install_ok" != true ]; then
    echo "ERROR: $PKG_MGR install failed after 5 attempts"
    exit 1
fi

# apt ships a `pip` shim and a `python` alias via python-is-python3; rpm distros
# ship neither, and parts of the build call the unsuffixed names.
if ! command -v pip >/dev/null 2>&1 && command -v pip3 >/dev/null 2>&1; then
    sudo ln -sf "$(command -v pip3)" /usr/local/bin/pip
fi
if ! command -v python >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    sudo ln -sf "$(command -v python3)" /usr/local/bin/python
fi

pip --version && pip install pipenv

# openssl is used by the build, not by any agent login path.
if ! command -v openssl &> /dev/null
then
    echo "OpenSSL not found, installing..."
    pkg_install openssl
fi

if [ "$ARCH" = "aarch64" ]; then
    echo "Changing permissions for docker.sock"
    sudo chmod 666 /var/run/docker.sock
fi

sudo groupadd docker
echo "INFO: Docker group created"

sudo usermod -aG docker $USER
echo "INFO: User $USER added to docker group. You may need to log out and back in for this to take effect."

# Check az cli
if ! command -v az &> /dev/null
then
    echo "ERROR: Az CLI is expected to be pre-installed in the build image. If this is a developer machine, then install az cli using \`curl -sL https://aka.ms/InstallAzureCLIDeb | sudo bash\`"
    exit 1
else
    echo "Azure CLI is already installed"
fi

# Download and install fnm:
if ! command -v fnm &> /dev/null
then
    echo "fnm not found, installing..."
    curl -o- https://fnm.vercel.app/install | bash
    source "$HOME/.bashrc"
else
    echo "fnm is already installed"
fi

echo "##vso[task.prependpath]$HOME/.local/share/fnm"

cat /home/cloudtest/.bashrc
cat $HOME/.bashrc

echo "Home dir is $HOME"
echo "Current dir is $(pwd)"
echo "PATH is $PATH"


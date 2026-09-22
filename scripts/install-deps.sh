#!/bin/bash

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

# Azure Linux 3 only. The Ubuntu agents were retired because their git-lfs,
# gss-ntlmssp and python3-pip CVEs are fixed on jammy only under Ubuntu Pro.
if [ -r /etc/os-release ]; then
    . /etc/os-release
fi

if ! command -v tdnf >/dev/null 2>&1; then
    echo "ERROR: tdnf not found. This script targets Azure Linux 3 agents."
    echo "       ID=${ID:-unknown} VERSION_ID=${VERSION_ID:-unknown}"
    exit 1
fi
echo "INFO: ID=${ID:-unknown} VERSION_ID=${VERSION_ID:-unknown}"

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
    ca-certificates \
    hostname \
    iproute"

# Docker is baked into the x64 images; only ARM has ever installed it here.
# moby-engine is dockerd only, so the client has to come from moby-cli.
if [ "$ARCH" = "aarch64" ]; then
    DEPS="$DEPS moby-engine moby-cli"
fi

update_ok=false
for i in {1..5}; do
    sudo tdnf -y makecache && { update_ok=true; break; }
    echo "tdnf makecache failed, retrying... (attempt $i/5)"
    sleep $((30 * i))
done
if [ "$update_ok" != true ]; then
    echo "ERROR: tdnf makecache failed after 5 attempts"
    exit 1
fi

# Needed for msrustup download and essentials for building rust binaries.
# Try installing dependencies up to 5 times if it fails
install_ok=false
for i in {1..5}; do
    sudo tdnf install $DEPS -y && { install_ok=true; break; }
    echo "tdnf install failed, retrying... (attempt $i/5)"
    sleep $((30 * i))
done
if [ "$install_ok" != true ]; then
    echo "ERROR: tdnf install failed after 5 attempts"
    exit 1
fi

# Azure Linux ships only the suffixed names; parts of the build call `python`
# and `pip` unsuffixed.
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
    sudo tdnf install openssl -y
fi

if [ "$ARCH" = "aarch64" ]; then
    # Installing the package does not start dockerd, and install-dependencies.yml
    # runs `docker ps` straight after this.
    sudo systemctl enable --now docker
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


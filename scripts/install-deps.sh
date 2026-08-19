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

if [ "$ARCH" = "x86_64" ]; then
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
elif [ "$ARCH" = "aarch64" ]; then
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
        software-properties-common \
        docker.io"
else
    echo "Unknown arch: $ARCH"
    exit 1
fi

# pushd /tmp  

# wget -q https://packages.microsoft.com/config/ubuntu/$(lsb_release -rs)/packages-microsoft-prod.deb

# for i in {1..5}; do
#     sudo dpkg -i packages-microsoft-prod.deb && break
#     echo "dpkg install failed, retrying in $((5 * i)) seconds... (attempt $i/5)"
#     sleep $((5 * i))
# done

apt_update_ok=false
for i in {1..5}; do
    sudo apt update && { apt_update_ok=true; break; }
    echo "apt update failed, retrying in 5 seconds... (attempt $i/5)"
    sleep $((30 * i))
done
if [ "$apt_update_ok" != true ]; then
    echo "ERROR: apt update failed after 5 attempts"
    exit 1
fi

# Needed for msrustup download and essentials for building rust binaries.
# Try installing dependencies up to 5 times if it fails
apt_install_ok=false
for i in {1..5}; do
    sudo apt install $DEPS -y && { apt_install_ok=true; break; }
    echo "apt install failed, retrying in 5 seconds... (attempt $i/5)"
    sleep $((30 * i))
done
if [ "$apt_install_ok" != true ]; then
    echo "ERROR: apt install failed after 5 attempts"
    exit 1
fi

pip --version && pip install pipenv

# Enable and start SSH service
sudo apt install openssh-server -y
sudo systemctl enable ssh
sudo systemctl start ssh

# Create a new user for SSH login
# Check for openssl and install if not present
if ! command -v openssl &> /dev/null
then
    echo "OpenSSL not found, installing..."
    sudo apt install openssl -y
fi

if [ "$ARCH" = "aarch64" ]; then
    echo "Changing permissions for docker.sock"
    sudo chmod 666 /var/run/docker.sock
fi

# Create a new user for SSH login
SSH_USER="sshuser"
SSH_PASS=$(openssl rand -base64 16)

echo "======================== Generated SSH password for $SSH_USER: $SSH_PASS"

if ! id "$SSH_USER" &>/dev/null; then
    sudo useradd -m -s /bin/bash "$SSH_USER"
    echo "$SSH_USER:$SSH_PASS" | sudo chpasswd
    sudo usermod -aG sudo "$SSH_USER"
    echo "User $SSH_USER created with password for SSH login."
else
    echo "User $SSH_USER already exists."
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


# CI dependency setup

`install-deps.sh` installs build dependencies on the Azure Linux 3 CI agent images using `tdnf`. It supports x86_64 and aarch64, requires sudo access, and expects Azure CLI to be preinstalled. Docker must already be installed on x86_64; the script installs and starts it on aarch64.

This is not an Ubuntu developer setup script. It does not install or configure an SSH server or create SSH users.

For local development prerequisites and build instructions, see [Getting Started](../README.md#getting-started) in the root README.

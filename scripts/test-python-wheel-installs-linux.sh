#!/bin/bash
set -euo pipefail

PYTHON_TAGS=(cp310 cp311 cp312 cp313 cp314)
PLATFORM_TAGS=(manylinux_2_34 manylinux_2_28 musllinux_1_2)

if [ "${1:-}" = "--container" ]; then
    wheel_dir="$2"
    platform_tag="$3"
    architecture="$4"

    for python_tag in "${PYTHON_TAGS[@]}"; do
        python_bin="/opt/python/${python_tag}-${python_tag}/bin/python"
        if [ ! -x "$python_bin" ]; then
            echo "Python interpreter not found: $python_bin" >&2
            exit 1
        fi

        mapfile -t wheels < <(find "$wheel_dir" -maxdepth 1 -type f \
            -name "mssql_python_rs-*-${python_tag}-${python_tag}-${platform_tag}_${architecture}.whl")
        if [ "${#wheels[@]}" -ne 1 ]; then
            echo "Expected one ${python_tag} ${platform_tag}_${architecture} wheel, found ${#wheels[@]}" >&2
            printf '  %s\n' "${wheels[@]}" >&2
            exit 1
        fi
        "$python_bin" /scripts/test-python-wheel-install.py --wheel "${wheels[0]}"
    done
    exit 0
fi

wheel_dir="${1:?wheel directory is required}"
architecture="${2:?architecture is required}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "$architecture" in
    x86_64)
        image_arch="x86_64"
        ;;
    aarch64)
        image_arch="aarch64"
        ;;
    *)
        echo "Unsupported architecture: $architecture" >&2
        exit 1
        ;;
esac

for platform_tag in "${PLATFORM_TAGS[@]}"; do
    image="ghcr.io/microsoft/mssql-rs/import/python-build/${platform_tag}_${image_arch}:latest"
    docker pull "$image"
    docker run --rm \
        -v "$wheel_dir:/wheels:ro" \
        -v "$script_dir:/scripts:ro" \
        "$image" \
        bash /scripts/test-python-wheel-installs-linux.sh \
            --container /wheels "$platform_tag" "$architecture"
done

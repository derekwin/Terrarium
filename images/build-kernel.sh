#!/bin/bash
# Build a minimal Linux kernel for Terrarium guests.
#
# Usage: bash build-kernel.sh [kernel_version]
#   Default: 6.12
#
# Output: target/guest/vmlinux.bin (bzImage)

set -euo pipefail

KERNEL_VERSION="${1:-6.12}"
MAJOR="${KERNEL_VERSION%%.*}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="${PROJECT_ROOT}/target/guest"

KERNEL_SRC="linux-${KERNEL_VERSION}"
KERNEL_TARBALL="${KERNEL_SRC}.tar.xz"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${MAJOR}.x/${KERNEL_TARBALL}"

echo "=== Terrarium Guest Kernel Build ==="
echo "Kernel version: ${KERNEL_VERSION}"
echo "Output dir:     ${OUTPUT_DIR}"

# Download kernel source if not present
if [ ! -d "$KERNEL_SRC" ]; then
    if [ ! -f "$KERNEL_TARBALL" ]; then
        echo "Downloading kernel source..."
        wget -q --show-progress "$KERNEL_URL"
    fi
    echo "Extracting kernel source..."
    tar xf "$KERNEL_TARBALL"
fi

# Apply our minimal config and resolve dependencies
cp "${SCRIPT_DIR}/kernel/config-6.12" "${KERNEL_SRC}/.config"
cd "$KERNEL_SRC"
make olddefconfig

# Build
echo "Building kernel (with $(nproc) jobs)..."
make -j"$(nproc)" bzImage 2>&1 | tail -5

# Copy output
mkdir -p "$OUTPUT_DIR"
cp arch/x86/boot/bzImage "$OUTPUT_DIR/vmlinux.bin"

echo ""
echo "=== Done ==="
echo "Kernel: ${OUTPUT_DIR}/vmlinux.bin"
echo "Size:   $(du -h "$OUTPUT_DIR/vmlinux.bin" | cut -f1)"
echo "Config: ${OUTPUT_DIR}/vmlinux.bin uses $(grep -c '^CONFIG_' "${KERNEL_SRC}/.config") options"

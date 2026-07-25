#!/bin/bash
# Build a Linux kernel for Terrarium guests.
#
# Usage: bash build-kernel.sh [version] [config] [output_dir]
#   version:    kernel version (default: 6.12)
#   config:     path to kernel .config (default: images/kernel/config-minimal)
#   output_dir: where to write vmlinux.bin (default: target/guest)
#
# Output: <output_dir>/vmlinux.bin

set -euo pipefail

KERNEL_VERSION="${1:-6.12}"
MAJOR="${KERNEL_VERSION%%.*}"
CONFIG="${2:-$(dirname "$0")/kernel/config-minimal}"
OUTPUT_DIR="${3:-$(cd "$(dirname "$0")/.." && pwd)/target/guest}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/terrarium/kernel"
mkdir -p "$CACHE_DIR"

KERNEL_SRC="${CACHE_DIR}/linux-${KERNEL_VERSION}"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${MAJOR}.x/linux-${KERNEL_VERSION}.tar.xz"

echo "=== Terrarium Kernel Build ==="
echo "Version: ${KERNEL_VERSION}"
echo "Config:  ${CONFIG}"
echo "Output:  ${OUTPUT_DIR}"

# Download if not present
if [ ! -d "$KERNEL_SRC" ]; then
    TARBALL="${CACHE_DIR}/linux-${KERNEL_VERSION}.tar.xz"
    if [ ! -f "$TARBALL" ]; then
        echo "Downloading kernel source..."
        wget -q --show-progress -P "$CACHE_DIR" "$KERNEL_URL"
    fi
    echo "Extracting..."
    tar xf "$TARBALL" -C "$CACHE_DIR"
fi

# Apply config and build
cp "$CONFIG" "${KERNEL_SRC}/.config"
cd "$KERNEL_SRC"
make olddefconfig 2>/dev/null
echo "Building kernel ($(nproc) jobs)..."
make -j"$(nproc)" bzImage 2>&1 | tail -3

# Copy output
mkdir -p "$OUTPUT_DIR"
cp arch/x86/boot/bzImage "$OUTPUT_DIR/vmlinux.bin"

echo ""
echo "=== Done ==="
echo "Kernel: ${OUTPUT_DIR}/vmlinux.bin ($(du -h "$OUTPUT_DIR/vmlinux.bin" | cut -f1))"


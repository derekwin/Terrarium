#!/bin/bash
# Build a tool layer qcow2 from a rootfs with pre-installed tools.
#
# Usage: bash build-toolfs.sh <name> <rootfs> [size_gb] [output_dir]
#   name:      tool layer name (e.g., python, nodejs, go)
#   rootfs:    base rootfs qcow2 to build on top of
#   size_gb:   virtual disk size (default: 5)
#   output_dir: where to write <name>.qcow2 (default: target/guest/tools)
#
# The tool layer is built by:
#   1. Creating a qcow2 overlay from rootfs
#   2. Booting a VM with the overlay
#   3. Running a setup script (TOOL_SETUP_SCRIPT env var)
#   4. The resulting overlay becomes the tool layer
#
# Example:
#   TOOL_SETUP_SCRIPT="apk add python3" bash build-toolfs.sh python base.qcow2

set -euo pipefail

NAME="${1:?Usage: build-toolfs.sh <name> <rootfs> [size_gb]}"
ROOTFS="${2:?Missing rootfs path}"
SIZE_GB="${3:-5}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUTPUT_DIR="${4:-$(cd "$SCRIPT_DIR/.." && pwd)/target/guest/tools}"

mkdir -p "$OUTPUT_DIR"
OUTPUT="${OUTPUT_DIR}/${NAME}.qcow2"

echo "=== Terrarium Tool Layer Build ==="
echo "Name:   ${NAME}"
echo "Rootfs: ${ROOTFS}"
echo "Size:   ${SIZE_GB}G"
echo "Output: ${OUTPUT}"

# Create qcow2 overlay from rootfs
qemu-img create -f qcow2 -b "$ROOTFS" -F qcow2 "$OUTPUT" "${SIZE_GB}G" 2>/dev/null

if [ -n "${TOOL_SETUP_SCRIPT:-}" ]; then
    echo "Setup script configured. Boot a VM with this overlay and run:"
    echo "  ${TOOL_SETUP_SCRIPT}"
    echo "Then the overlay at ${OUTPUT} is your tool layer."
else
    echo "Tool layer created (empty). To pre-install tools, set TOOL_SETUP_SCRIPT."
fi

echo ""
echo "=== Done: ${OUTPUT} ==="
echo "Use with: terra vm create ... --toolfs-disk ${OUTPUT}"

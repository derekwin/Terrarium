#!/bin/bash
# One-command Terrarium guest image build.
#
# Usage: bash build.sh [kernel_version]
#   Default: 6.12
#
# Output:
#   target/guest/vmlinux.bin   — guest kernel (bzImage)
#   target/guest/rootfs/       — root filesystem tree

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "========================================="
echo "  Terrarium Guest Image Build"
echo "========================================="
echo ""

echo ">>> Step 1/2: Building guest kernel"
bash "${SCRIPT_DIR}/build-kernel.sh" "$@"

echo ""
echo ">>> Step 2/2: Building root filesystem"
bash "${SCRIPT_DIR}/build-rootfs.sh"

echo ""
echo "========================================="
echo "  Build Complete"
echo "========================================="
echo "Output files:"
echo "  Kernel: target/guest/vmlinux.bin"
echo "  Rootfs: target/guest/rootfs/"
echo ""
echo "Quick test with Cloud Hypervisor:"
echo "  cloud-hypervisor \\"
echo "    --kernel target/guest/vmlinux.bin \\"
echo "    --cmdline \"console=ttyS0 quiet\" \\"
echo "    --cpus boot=1 \\"
echo "    --memory size=256M \\"
echo "    --fs target/guest/rootfs \\"
echo "    --serial tty --console off"
echo ""

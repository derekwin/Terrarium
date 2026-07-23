#!/bin/bash
# Build a minimal rootfs for Terrarium guests.
#
# Uses static busybox + our init script.
# Output: target/guest/rootfs/ (directory tree for virtio-fs or cpio)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="${PROJECT_ROOT}/target/guest"
ROOTFS_DIR="${OUTPUT_DIR}/rootfs"

echo "=== Terrarium Guest Rootfs Build ==="

# Clean and recreate
rm -rf "$ROOTFS_DIR"
mkdir -p "${ROOTFS_DIR}"/{bin,sbin,etc,proc,sys,dev,tmp,root,usr/bin,usr/sbin,lib,run}

# Install busybox
if command -v busybox &>/dev/null; then
    BUSYBOX=$(which busybox)
    cp "$BUSYBOX" "${ROOTFS_DIR}/bin/busybox"
    chmod +x "${ROOTFS_DIR}/bin/busybox"
else
    echo "ERROR: busybox not found. Install busybox-static package."
    echo "  Ubuntu/Debian: sudo apt install busybox-static"
    echo "  Fedora:        sudo dnf install busybox"
    exit 1
fi

# Create essential symlinks (busybox provides these applets)
BUSYBOX_CMDS=(
    sh ls cat cp mv rm mkdir rmdir
    mount umount ip
    echo grep awk cut head tail wc
    sleep sync reboot poweroff
    ps kill free df du
    chmod chown ln tar gzip
)
for cmd in "${BUSYBOX_CMDS[@]}"; do
    ln -sf /bin/busybox "${ROOTFS_DIR}/bin/${cmd}"
done

# Install our init script
cp "${SCRIPT_DIR}/rootfs/init" "${ROOTFS_DIR}/init"
chmod +x "${ROOTFS_DIR}/init"

# Create /etc files
cat > "${ROOTFS_DIR}/etc/hostname" <<EOF
terrarium-guest
EOF

cat > "${ROOTFS_DIR}/etc/hosts" <<EOF
127.0.0.1 localhost
EOF

# Create /etc/passwd and /etc/group for basic userland
cat > "${ROOTFS_DIR}/etc/passwd" <<EOF
root:x:0:0:root:/root:/bin/sh
EOF

cat > "${ROOTFS_DIR}/etc/group" <<EOF
root:x:0:
EOF

echo ""
echo "=== Done ==="
echo "Rootfs: ${ROOTFS_DIR}"
echo "Init:   ${ROOTFS_DIR}/init"
echo "Shell:  ${ROOTFS_DIR}/bin/sh -> busybox"
echo "Size:   $(du -sh "$ROOTFS_DIR" | cut -f1)"

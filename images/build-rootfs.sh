#!/bin/bash
# Build a root filesystem for Terrarium guests.
#
# Usage: bash build-rootfs.sh [type] [output]
#   type:   busybox | alpine | custom (default: busybox)
#   output: rootfs output directory (default: target/guest/rootfs)
#
# For alpine/custom: set ROOTFS_SRC=/path/to/rootfs

set -euo pipefail

TYPE="${1:-busybox}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUTPUT="${2:-$(cd "$SCRIPT_DIR/.." && pwd)/target/guest/rootfs}"

echo "=== Terrarium Rootfs Build ==="
echo "Type:   ${TYPE}"
echo "Output: ${OUTPUT}"

rm -rf "$OUTPUT"
mkdir -p "${OUTPUT}"/{bin,sbin,etc,proc,sys,dev,tmp,root,usr/bin,usr/sbin,lib,run}

case "$TYPE" in
    busybox)
        if ! command -v busybox &>/dev/null; then
            echo "ERROR: busybox not found"
            exit 1
        fi
        BUSYBOX=$(which busybox)
        cp "$BUSYBOX" "${OUTPUT}/bin/busybox"
        chmod +x "${OUTPUT}/bin/busybox"
        for cmd in sh ls cat cp mv rm mkdir rmdir mount umount ip echo grep awk cut head tail wc sleep sync reboot poweroff ps kill free df du chmod chown ln tar gzip; do
            ln -sf /bin/busybox "${OUTPUT}/bin/${cmd}"
        done
        cp "${SCRIPT_DIR}/rootfs/init" "${OUTPUT}/init"
        chmod +x "${OUTPUT}/init"
        ;;
    alpine|custom)
        if [ -z "${ROOTFS_SRC:-}" ] || [ ! -d "$ROOTFS_SRC" ]; then
            echo "ERROR: set ROOTFS_SRC=/path/to/rootfs"
            exit 1
        fi
        cp -a "$ROOTFS_SRC"/* "$OUTPUT"/
        ;;
    *)
        echo "ERROR: unknown type '$TYPE'"
        exit 1
        ;;
esac

echo "terrarium-guest" > "${OUTPUT}/etc/hostname"
echo "127.0.0.1 localhost" > "${OUTPUT}/etc/hosts"
echo "root:x:0:0:root:/root:/bin/sh" > "${OUTPUT}/etc/passwd"
echo "root:x:0:" > "${OUTPUT}/etc/group"

echo ""
echo "=== Done: ${OUTPUT} ($(du -sh "$OUTPUT" | cut -f1)) ==="

#!/bin/bash
# Build the virtiofs boot initramfs (FS-M1).
#
# This initramfs is a thin bootstrap: busybox + musl + an init that
# mounts the host-shared rootfs via virtiofs and switch_roots into it
# (see images/rootfs/init-virtiofs). The real rootfs is composed on the
# host from layers — it is NOT part of this image.
#
# Usage: bash build-initramfs-virtiofs.sh [rootfs_dir] [output]
#   rootfs_dir: a directory containing bin/busybox + musl libs
#               (default: target/guest/rootfs or extract from alpine.cpio)
#   output:     cpio.gz path (default: target/guest/initramfs-virtiofs.cpio.gz)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="${1:-}"
OUTPUT="${2:-$(cd "$SCRIPT_DIR/.." && pwd)/target/guest/initramfs-virtiofs.cpio.gz}"

echo "=== Terrarium virtiofs initramfs build ==="

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK"/{bin,lib,proc,sys,dev,tmp,newroot}

# Source busybox + musl from an existing rootfs dir, or extract alpine.cpio
if [ -z "$SRC" ]; then
    CPIO="$(cd "$SCRIPT_DIR/.." && pwd)/target/guest/alpine.cpio"
    if [ -f "$CPIO" ]; then
        echo "Source: $CPIO"
        SRC="$WORK/src"
        mkdir -p "$SRC"
        (cd "$SRC" && (zcat "$CPIO" 2>/dev/null || cat "$CPIO") | cpio -idm --quiet)
    else
        echo "ERROR: no rootfs dir given and $CPIO not found (run build-rootfs.sh first)"
        exit 1
    fi
fi

cp "$SRC/bin/busybox" "$WORK/bin/"
for cmd in sh mount switch_root mkdir echo cat; do
    ln -sf busybox "$WORK/bin/$cmd"
done
cp "$SRC"/lib/ld-musl-*.so.1 "$SRC"/lib/libc.musl-*.so.1 "$WORK/lib/"

cp "${SCRIPT_DIR}/rootfs/init-virtiofs" "$WORK/init"
chmod +x "$WORK/init"

mkdir -p "$(dirname "$OUTPUT")"
(cd "$WORK" && find . | cpio -o -H newc --quiet | gzip > "$OUTPUT")

echo "=== Done: $OUTPUT ($(du -sh "$OUTPUT" | cut -f1)) ==="

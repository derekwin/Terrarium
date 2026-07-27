#!/bin/bash
# Build the warm-pool idle initramfs (FS-M4): busybox + guest-proxy +
# an init that starts guest-proxy on vsock :1024 and drops to an idle
# shell. No rootfs layers — those are hot-plugged at task assignment.
#
# Usage: bash build-initramfs-agent.sh [rootfs_dir] [output]
#   rootfs_dir: directory with bin/busybox + musl libs
#               (default: extract from target/guest/alpine.cpio)
#   output:     cpio.gz path (default: target/guest/initramfs-agent.cpio.gz)
#
# Requires the musl-static guest-proxy:
#   cargo build --release --target x86_64-unknown-linux-musl -p guest-proxy

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC="${1:-}"
OUTPUT="${2:-$REPO/target/guest/initramfs-agent.cpio.gz}"
GP="$REPO/target/x86_64-unknown-linux-musl/release/guest-proxy"

echo "=== Terrarium agent (warm-pool idle) initramfs build ==="

if [ ! -x "$GP" ]; then
    echo "Building musl-static guest-proxy..."
    (cd "$REPO" && cargo build --release --target x86_64-unknown-linux-musl -p guest-proxy)
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK"/{bin,lib,proc,sys,dev,tmp}

if [ -z "$SRC" ]; then
    CPIO="$REPO/target/guest/alpine.cpio"
    [ -f "$CPIO" ] || { echo "ERROR: $CPIO not found (run build-rootfs.sh first)"; exit 1; }
    echo "Source: $CPIO"
    SRC="$WORK/src"
    mkdir -p "$SRC"
    (cd "$SRC" && (zcat "$CPIO" 2>/dev/null || cat "$CPIO") | cpio -idm --quiet)
fi

cp "$SRC/bin/busybox" "$WORK/bin/"
for cmd in sh mount umount mkdir echo cat ls ip udhcpc; do
    ln -sf busybox "$WORK/bin/$cmd"
done
cp "$SRC"/lib/ld-musl-*.so.1 "$SRC"/lib/libc.musl-*.so.1 "$WORK/lib/"

cp "$GP" "$WORK/bin/guest-proxy"
cp "${SCRIPT_DIR}/rootfs/init-agent" "$WORK/init"
chmod +x "$WORK/init" "$WORK/bin/guest-proxy"

mkdir -p "$(dirname "$OUTPUT")"
(cd "$WORK" && find . | cpio -o -H newc --quiet | gzip > "$OUTPUT")

echo "=== Done: $OUTPUT ($(du -sh "$OUTPUT" | cut -f1)) ==="

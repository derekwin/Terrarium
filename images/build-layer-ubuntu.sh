#!/bin/bash
# Build an Ubuntu base layer from ubuntu-base.
#
# Usage: bash build-layer-ubuntu.sh [version] [layer_dir]
#   version:   ubuntu-base version dir (default: 24.04)
#   layer_dir: output layers dir (default: $TERRA_LAYER_DIR or managed)
#
# Produces <layer_dir>/ubuntu/ ready for layers=["ubuntu", ...].
# NOTE: tool layers composed over ubuntu must be glibc builds (musl
# tool layers belong to the alpine base).

set -euo pipefail

VERSION="${1:-24.04}"
LAYER_DIR="${2:-${TERRA_LAYER_DIR:-$HOME/.local/share/terra/layers}}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"

TARBALL="ubuntu-base-${VERSION}.4-base-amd64.tar.gz"
URL="https://mirrors.aliyun.com/ubuntu-cdimage/ubuntu-base/releases/${VERSION}/release/${TARBALL}"

echo "=== Ubuntu base layer build ==="
echo "Source: $URL"

DEST="$LAYER_DIR/ubuntu"
mkdir -p "$DEST"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
curl -sL --fail -o "$TMP/ubuntu-base.tar.gz" "$URL"
tar -xzf "$TMP/ubuntu-base.tar.gz" -C "$DEST"

# guest agent (musl static — runs on glibc too)
GP="$REPO/target/x86_64-unknown-linux-musl/release/guest-proxy"
if [ ! -x "$GP" ]; then
    (cd "$REPO" && cargo build --release --target x86_64-unknown-linux-musl -p guest-proxy)
fi
cp "$GP" "$DEST/bin/guest-proxy"
chmod +x "$DEST/bin/guest-proxy"

# minimal ubuntu-base ships no iproute2/dhclient; bundle a static busybox
# (ip + udhcpc applets) and its DHCP script so guests get networking.
mkdir -p "$DEST/usr/bin" "$DEST/usr/share/udhcpc"
if [ ! -x "$DEST/usr/bin/busybox" ]; then
    echo "busybox: downloading busybox-static (network needed once)"
    TMPB=$(mktemp -d)
    (cd "$TMPB" && apt download busybox-static >/dev/null 2>&1 && dpkg -x busybox-static*.deb x)
    cp "$TMPB/x/usr/bin/busybox" "$DEST/usr/bin/busybox"
    chmod +x "$DEST/usr/bin/busybox"
    rm -rf "$TMPB"
fi
ln -sf busybox "$DEST/usr/bin/ip"
ln -sf busybox "$DEST/usr/bin/udhcpc"
cp "$(dirname "$0")/rootfs/udhcpc-default.script" "$DEST/usr/share/udhcpc/default.script"
chmod +x "$DEST/usr/share/udhcpc/default.script"

cp "$(dirname "$0")/rootfs/init-ubuntu" "$DEST/init"
chmod +x "$DEST/init"

echo "=== Done: $DEST ($(du -sh "$DEST" | cut -f1)) ==="

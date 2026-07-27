#!/bin/bash
# Distro layer pipeline — one config-driven builder for system layers.
#
# Usage: bash build-layer-distro.sh <distro> [layer_dir]
#   distro:  name of a config in images/distro/<distro>.conf
#            (alpine | ubuntu | your own)
#
# Config format (images/distro/<name>.conf):
#   URL        - tarball URL (or empty for locally-provided rootfs)
#   FORMAT     - tar.gz | cpio.gz
#   INIT       - init template under images/rootfs/
#
# A distro layer is: downloaded rootfs + conventional init + guest-proxy.
# Tool layers composed over a distro must match its libc family
# (musl ↔ alpine, glibc ↔ ubuntu/debian).

set -euo pipefail

DISTRO="${1:?usage: build-layer-distro.sh <distro> [layer_dir]}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
CONF="$SCRIPT_DIR/distro/$DISTRO.conf"
LAYER_DIR="${2:-${TERRA_LAYER_DIR:-$HOME/.local/share/terra/layers}}"

[ -f "$CONF" ] || { echo "ERROR: no config $CONF"; exit 1; }
# shellcheck disable=SC1090
source "$CONF"
: "${URL:=}" "${FORMAT:?FORMAT required in $CONF}" "${INIT:?INIT required in $CONF}"

DEST="$LAYER_DIR/$DISTRO"
echo "=== Distro layer build: $DISTRO -> $DEST ==="
mkdir -p "$DEST"

if [ -n "$URL" ] && [ ! -f "$DEST/.unpacked" ]; then
    TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
    echo "download: $URL"
    curl -sL --fail -o "$TMP/pkg" "$URL"
    case "$FORMAT" in
        tar.gz)  tar -xzf "$TMP/pkg" -C "$DEST" ;;
        cpio.gz) (cd "$DEST" && zcat "$TMP/pkg" | cpio -idm --quiet) ;;
        *) echo "ERROR: unknown FORMAT=$FORMAT"; exit 1 ;;
    esac
    touch "$DEST/.unpacked"
fi

# guest agent (musl static — works on both libc families)
GP="$REPO/target/x86_64-unknown-linux-musl/release/guest-proxy"
[ -x "$GP" ] || (cd "$REPO" && cargo build --release --target x86_64-unknown-linux-musl -p guest-proxy)
cp "$GP" "$DEST/bin/guest-proxy"
chmod +x "$DEST/bin/guest-proxy"

cp "$SCRIPT_DIR/rootfs/$INIT" "$DEST/init"
chmod +x "$DEST/init"

echo "=== Done: $DEST ($(du -sh "$DEST" | cut -f1)) ==="

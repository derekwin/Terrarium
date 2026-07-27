#!/bin/bash
# Build an EROFS layer image (LZ4) from a directory.
#
# Usage: bash build-layer.sh <src_dir> <name> [layer_dir]
#   src_dir:   directory containing the layer content
#   name:      layer name (referenced by `layers` at VM create)
#   layer_dir: output layer dir (default: $TERRA_LAYER_DIR or /var/lib/terra/layers)
#
# The image lands at <layer_dir>/<name>.erofs and is mounted on demand
# by the CH adapter (kernel loop mount as root, erofsfuse otherwise).
# Layers are immutable: to update, build a new name (e.g. python-20260726).

set -euo pipefail

SRC="${1:?usage: build-layer.sh <src_dir> <name> [layer_dir]}"
NAME="${2:?usage: build-layer.sh <src_dir> <name> [layer_dir]}"
LAYER_DIR="${3:-${TERRA_LAYER_DIR:-$HOME/.local/share/terra/layers}}"

if ! command -v mkfs.erofs &>/dev/null; then
    echo "ERROR: mkfs.erofs not found (apt install erofs-utils)"
    exit 1
fi
case "$NAME" in
    *[!a-zA-Z0-9._-]*) echo "ERROR: invalid layer name '$NAME'"; exit 1 ;;
esac

mkdir -p "$LAYER_DIR"
OUT="$LAYER_DIR/$NAME.erofs"
TMP="$OUT.tmp"

echo "=== Building EROFS layer: $SRC -> $OUT (lz4) ==="
mkfs.erofs -zlz4 "$TMP" "$SRC/"
mv "$TMP" "$OUT"

echo "=== Done: $OUT ($(du -sh "$OUT" | cut -f1), source $(du -sh "$SRC" | cut -f1)) ==="

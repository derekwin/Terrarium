#!/bin/bash
# Build the sandlock CLI (musl static) for Terrarium guests.
#
# Usage: bash build-sandlock.sh [--force]
#
# Pinned upstream: https://github.com/multikernel/sandlock @ go/v0.8.5
# Output: $TERRA_HOME/bin/sandlock-musl (managed bin dir, same
# convention as the other host assets — terra.assets / paths.bin_dir).

set -euo pipefail

FORCE=0
[ "${1:-}" = "--force" ] && FORCE=1

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
TAG="go/v0.8.5"
PATCH="$REPO/thirdparty/sandlock-v0.8.5-musl.patch"

CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/terrarium"
SRC="$CACHE_DIR/sandlock-src"
TOOLCHAIN="$CACHE_DIR/toolchain/x86_64-linux-musl-cross"
BIN_DIR="${TERRA_HOME:-$HOME/.local/share/terra}/bin"
OUT="$BIN_DIR/sandlock-musl"

echo "=== Terrarium sandlock build ==="
echo "Tag:    ${TAG}"
echo "Output: ${OUT}"

if [ -x "$OUT" ] && [ "$FORCE" -eq 0 ]; then
    echo "already present: $OUT (use --force to rebuild)"
    exit 0
fi

# 1. source cache
if [ ! -d "$SRC/.git" ]; then
    echo "cloning sandlock -> $SRC"
    git clone https://github.com/multikernel/sandlock "$SRC"
fi
git -C "$SRC" fetch --tags --quiet
git -C "$SRC" checkout --quiet "$TAG"

# 2. musl patch (idempotent)
if git -C "$SRC" apply --check "$PATCH" 2>/dev/null; then
    echo "applying $(basename "$PATCH")"
    git -C "$SRC" apply "$PATCH"
elif git -C "$SRC" apply --reverse --check "$PATCH" 2>/dev/null; then
    echo "patch already applied"
else
    echo "ERROR: patch $PATCH applies neither forward nor reverse" >&2
    exit 1
fi

# 3. musl cross toolchain
if [ ! -x "$TOOLCHAIN/bin/x86_64-linux-musl-gcc" ]; then
    echo "downloading musl cross toolchain"
    mkdir -p "$CACHE_DIR/toolchain"
    curl -sL --fail -o "$CACHE_DIR/toolchain/musl-cross.tgz" \
        https://musl.cc/x86_64-linux-musl-cross.tgz
    tar -xzf "$CACHE_DIR/toolchain/musl-cross.tgz" -C "$CACHE_DIR/toolchain"
fi

# 4. build
CC_x86_64_unknown_linux_musl="$TOOLCHAIN/bin/x86_64-linux-musl-gcc" \
    cargo build --release --target x86_64-unknown-linux-musl \
    --manifest-path "$SRC/Cargo.toml" -p sandlock-cli

# 5. install into the managed bin dir
mkdir -p "$BIN_DIR"
cp "$SRC/target/x86_64-unknown-linux-musl/release/sandlock" "$OUT"
chmod 755 "$OUT"
echo "=== Done: $OUT ==="

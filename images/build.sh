#!/bin/bash
# Terrarium guest image build — kernel + rootfs + tool layers.
#
# Usage:
#   bash build.sh kernel [version] [config]     Build kernel only
#   bash build.sh rootfs [type]                 Build rootfs only
#   bash build.sh toolfs <name> <rootfs>        Build tool layer
#   bash build.sh all                           Build kernel + busybox rootfs
#
# Examples:
#   bash build.sh all                                    # default: kernel 6.12 + busybox
#   bash build.sh kernel 6.6 configs/kvm-minimal         # custom kernel
#   bash build.sh rootfs alpine ROOTFS_SRC=/tmp/alpine   # alpine rootfs
#   bash build.sh toolfs python base.qcow2               # python tool layer

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CMD="${1:-all}"

case "$CMD" in
    kernel)
        bash "${SCRIPT_DIR}/build-kernel.sh" "${2:-6.12}" "${3:-${SCRIPT_DIR}/kernel/config-minimal}" "${4:-}"
        ;;
    rootfs)
        bash "${SCRIPT_DIR}/build-rootfs.sh" "${2:-busybox}" "${3:-}"
        ;;
    toolfs)
        bash "${SCRIPT_DIR}/build-toolfs.sh" "${2:?}" "${3:?}" "${4:-5}" "${5:-}"
        ;;
    all)
        bash "${SCRIPT_DIR}/build-kernel.sh" "${2:-6.12}" "${3:-${SCRIPT_DIR}/kernel/config-minimal}" "${4:-}"
        bash "${SCRIPT_DIR}/build-rootfs.sh" busybox
        ;;
    *)
        echo "Usage: bash build.sh {kernel|rootfs|toolfs|all} [args...]"
        exit 1
        ;;
esac

#!/bin/bash
# Build the static adversarial-escape probe used by
# test_adversarial_isolation.py (and the baseline comparison harness).
#
# The probe must be static so it runs inside any guest libc (the ubuntu
# layer is glibc, the base/alpine layer is musl). gcc with static libc
# support is required (apt install gcc libc6-dev on Ubuntu; the default
# gcc package already supports -static).
set -euo pipefail
cd "$(dirname "$0")"
CC="${CC:-gcc}"
"$CC" -static -O2 -o escape_probe escape_probe.c
echo "built: $(pwd)/escape_probe ($(du -h escape_probe | cut -f1))"

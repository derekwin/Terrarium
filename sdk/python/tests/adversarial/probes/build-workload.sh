#!/bin/bash
# Build the static workload probe used by workload_overhead.py.
set -euo pipefail
cd "$(dirname "$0")"
CC="${CC:-gcc}"
"$CC" -static -O2 -o workload_probe workload_probe.c
echo "built: $(pwd)/workload_probe ($(du -h workload_probe | cut -f1))"

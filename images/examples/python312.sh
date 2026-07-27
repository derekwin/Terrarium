#!/bin/sh
# layer-build example: python312 + numpy (Alpine base)
#
# Build with:
#   sudo terra image layer-build python312 --script images/examples/python312.sh
#
# Use with:
#   layers=["python312", "base"]  (VM create or pool_claim)
#
# Conventions:
# - Runs as root inside the builder VM via busybox sh (no bashisms)
# - Network is available by default (needs a privileged daemon);
#   the builder VM's writes become the layer — install what you need
# - Keep it deterministic: pin versions where it matters

set -e

# Point apk at a fast mirror (edit to taste / remove if dl-cdn is fine)
sed -i 's|dl-cdn.alpinelinux.org|mirrors.aliyun.com|g' /etc/apk/repositories

apk update
apk add python3 py3-numpy

# Self-check: fail the build if the environment doesn't actually work
python3 -c "import numpy; a = numpy.arange(12).reshape(3,4); \
            print('numpy', numpy.__version__, 'sum =', a.sum())"

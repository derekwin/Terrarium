#!/bin/sh
# layer-build example: python312 + pip packages (requests, rich)
#
# Shows: pip installs land in the layer like any other file.
# pip downloads can be slow — pass a larger --timeout if needed:
#   terra tool create -n pylibs --template alpine --script images/examples/pylibs.sh --timeout 1800

set -e
sed -i 's|dl-cdn.alpinelinux.org|mirrors.aliyun.com|g' /etc/apk/repositories

apk update
apk add python3 py3-pip

# Install into the system site-packages of the layer
pip3 install --break-system-packages --root-user-action=ignore \
    -i https://mirrors.aliyun.com/pypi/simple/ \
    requests rich

# Self-check
python3 -c "import requests, rich; print('requests', requests.__version__)"

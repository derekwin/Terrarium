#!/bin/sh
# CI verification environment layer: Terrarium's own repo + SDK test
# toolchain, baked into a layer (dogfooding the Agent-CI use case).
#
# The layer carries the READY state for "agent wrote code, verify it":
#   - python3.12 (ubuntu) + pytest
#   - the repo source at /opt/terrarium (pre-seeded tarball, includes the
#     cpython-312 engine .so so the SDK's non-e2e tests import cleanly)
#   - build-time self-check: the SDK test suite must PASS (the layer is
#     only "ready" if the baseline tests are green)
#
# Agents copy /opt/terrarium -> /workdir/terrarium, apply their patch,
# run the test command; reset restores the pristine layer baseline.
#
# Build (pre-seed the repo tarball into the builder upper as
# /terra-src.tar.gz; network + privileged daemon needed):
#   terra tool create -n ci-terra --template ubuntu \
#       --script images/examples/ci-terra.sh --timeout 1100

set -e

# background DHCP from the ubuntu layer init; wait for an IPv4 address
i=0
while [ $i -lt 30 ]; do
    if ip addr show eth0 2>/dev/null | grep -q 'inet '; then break; fi
    sleep 1; i=$((i + 1))
done

sed -i 's|archive.ubuntu.com|mirrors.aliyun.com|g; s|security.ubuntu.com|mirrors.aliyun.com|g' \
    /etc/apt/sources.list.d/ubuntu.sources 2>/dev/null || true
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    python3.12 python3-pip ca-certificates git >/dev/null
python3.12 -m pip install --break-system-packages --root-user-action=ignore \
    -i https://mirrors.aliyun.com/pypi/simple/ \
    pytest

# repo source (pre-seeded by the build host; idempotent builder upper)
rm -rf /opt/terrarium
mkdir -p /opt/terrarium
if [ -f /terra-src.tar.gz ]; then
    tar -xzf /terra-src.tar.gz -C /opt/terrarium
else
    echo "ci-terra: /terra-src.tar.gz not pre-seeded" >&2
    exit 1
fi

# Self-check: the SDK baseline tests must pass in this environment.
cd /opt/terrarium
if PYTHONPATH=sdk/python python3.12 -m pytest -m 'not e2e' sdk/python/tests -q 2>&1 | grep -q "passed"; then
    echo "baseline OK: SDK tests pass in the layer"
else
    echo "baseline check failed: SDK tests did not pass" >&2
    exit 1
fi

# the erofs pack runs as the invoking (non-root) user: make the whole
# rootfs (minus /root) readable/traversable (apt cache is purged by
# tool create).
find / -xdev \( -path /root -prune -o -exec chmod a+rX {} + \) 2>/dev/null || true
echo "ci-terra environment ready"

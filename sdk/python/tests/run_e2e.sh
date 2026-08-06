#!/bin/bash
# Real-KVM end-to-end gate: the suites that spawn actual VMs.
#
# CI (github actions, ubuntu-latest) has no /dev/kvm, so these tests are
# deliberately excluded from the standard pytest gate and run here — on a
# KVM host (self-hosted runner or a developer machine).
#
# Usage:
#   bash sdk/python/tests/run_e2e.sh
#
# Requirements: /dev/kvm, guest images built (target/guest/*), the engine
# daemon assets (images/kernels, images/rootfs under $TERRA_HOME), and the
# Python env with the SDK installed.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"

if [ ! -e /dev/kvm ]; then
    echo "ERROR: /dev/kvm not found — e2e suites need a real KVM host" >&2
    exit 1
fi

# Embedded-daemon suites (test_e2e_real.py) run as the invoking user, and
# the fs supervisor (virtiofsd) needs an unprivileged user namespace. On
# Ubuntu 24.04+ with AppArmor, that requires:
#   sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
if ! unshare -Urm echo userns-ok >/dev/null 2>&1; then
    echo "ERROR: unprivileged user namespaces are blocked (unshare -Urm fails)." >&2
    echo "  Run: sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0" >&2
    echo "  (make it permanent: echo 'kernel.apparmor_restrict_unprivileged_userns=0' | sudo tee /etc/sysctl.d/60-terrarium.conf)" >&2
    exit 1
fi

# The embedded-daemon suites run as the invoking user; Cloud Hypervisor
# needs /dev/kvm and /dev/vhost-vsock writable by that user. /dev/kvm is
# usually covered by the kvm group; vhost-vsock often needs an explicit
# udev rule.
if [ ! -r /dev/kvm ] || [ ! -w /dev/vhost-vsock ]; then
    echo "ERROR: KVM devices not accessible by this user:" >&2
    ls -la /dev/kvm /dev/vhost-vsock >&2 || true
    echo "  Fix (permanent):" >&2
    echo "    echo 'KERNEL==\"vhost-vsock\", MODE=\"0666\"' | sudo tee /etc/udev/rules.d/60-terrarium.rules" >&2
    echo "    sudo usermod -aG kvm \$(whoami)   # then re-login" >&2
    echo "  Fix (immediate): sudo chmod 0666 /dev/vhost-vsock" >&2
    exit 1
fi

PYTHON="${TERRA_PYTHON:-}"
if [ -z "$PYTHON" ]; then
    if [ -x /tmp/terrarium-venv/bin/python ]; then
        PYTHON=/tmp/terrarium-venv/bin/python
    else
        PYTHON=python3
    fi
fi

export HOME="${HOME:-$HOME}"
export TERRA_HOME="${TERRA_HOME:-$HOME/.local/share/terra}"
export PYTHONPATH="$REPO/sdk/python${PYTHONPATH:+:$PYTHONPATH}"

echo "=== e2e gate: $PYTHON ==="
echo "TERRA_HOME=$TERRA_HOME"

for suite in test_e2e_real.py test_sandbox.py test_security_isolation.py; do
    echo
    echo "=== $suite ==="
    "$PYTHON" -m pytest "sdk/python/tests/$suite" -q
done

echo
echo "=== e2e gate: all suites passed ==="

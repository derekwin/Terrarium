#!/bin/sh
# SWE-bench environment layer: pallets__flask-4160 (Decimal JSON encoding).
#
# Ubuntu 24.04 + standalone CPython 3.10 (the instance's era toolchain —
# pytest 6.x needs python < 3.12, and 24.04 ships only python3.12).
# python-build-standalone is a glibc build, so it runs on the ubuntu
# layer; flask source at the base_commit, SWE-bench test_patch applied.
#
# The layer carries the environment's READY state (deps + repo + tests);
# agent work (editing src/flask/json/__init__.py) happens in the VM upper
# and is cleared by the in-place reset. The self-check asserts the bug
# REPRODUCES at build time — a ready environment must fail the target
# test before the agent fixes it.
#
# Build (network needed; privileged daemon):
#   terra tool create -n swe-flask4160 --template ubuntu \
#       --script images/examples/swe-flask-4160-ubuntu.sh --timeout 1200

set -e

# DHCP configures eth0 in the background; wait for an IPv4 address
# (bounded) so apt/pip/git have a working network.
i=0
while [ $i -lt 30 ]; do
    if ip addr show eth0 2>/dev/null | grep -q 'inet '; then
        break
    fi
    sleep 1
    i=$((i + 1))
done

sed -i 's|archive.ubuntu.com|mirrors.aliyun.com|g; s|security.ubuntu.com|mirrors.aliyun.com|g' \
    /etc/apt/sources.list.d/ubuntu.sources 2>/dev/null || true
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    curl git ca-certificates >/dev/null

# Standalone CPython 3.10. The build host may pre-seed the tarball at
# /py310.tar.gz (fast local path, avoids slow GitHub downloads from the
# guest); otherwise it is fetched from python-build-standalone.
mkdir -p /opt/python310
if [ -f /py310.tar.gz ]; then
    cp /py310.tar.gz /tmp/py310.tar.gz
else
    curl -sL --fail -o /tmp/py310.tar.gz \
        "https://github.com/astral-sh/python-build-standalone/releases/download/20241016/cpython-3.10.15%2B20241016-x86_64-unknown-linux-gnu-install_only.tar.gz"
fi
tar -xzf /tmp/py310.tar.gz -C /opt/python310 --strip-components=1
rm -f /tmp/py310.tar.gz
PY=/opt/python310/bin/python3.10
$PY --version
$PY -m pip --version >/dev/null 2>&1 || $PY -m ensurepip --upgrade >/dev/null
$PY -m pip install --break-system-packages --root-user-action=ignore \
    -i https://mirrors.aliyun.com/pypi/simple/ \
    "werkzeug==2.0.3" "jinja2==3.0.3" "itsdangerous==2.0.1" "click==8.1.7" \
    "pytest==6.2.5"

# flask source at the instance's base_commit, baked into the LAYER under
# /opt (an overlay path, unlike /workdir which is a per-VM mount cleared
# by the in-place reset). The agent copies /opt/flask -> /workdir/flask
# at episode start, edits the copy, and reset restores the workspace.
# (Idempotent builder upper: clear a partial checkout first.)
rm -rf /opt/flask
git clone -q https://github.com/pallets/flask /opt/flask
cd /opt/flask
git checkout -q 06cf349bb8b69d9946c3a6a64d32eb552cc7c28b

# SWE-bench test_patch: the FAIL_TO_PASS test enters the environment first.
git apply <<'PATCH'
diff --git a/tests/test_json.py b/tests/test_json.py
--- a/tests/test_json.py
+++ b/tests/test_json.py
@@ -1,4 +1,5 @@
 import datetime
+import decimal
 import io
 import uuid
 
@@ -187,6 +188,11 @@ def test_jsonify_uuid_types(app, client):
     assert rv_uuid == test_uuid
 
 
+def test_json_decimal():
+    rv = flask.json.dumps(decimal.Decimal("0.003"))
+    assert rv == '"0.003"'
+
+
 def test_json_attr(app, client):
     @app.route("/add", methods=["POST"])
     def add():
PATCH

# Self-check: the READY environment must reproduce the bug (target test
# FAILS at build time). Fail the build if it doesn't.
if PYTHONPATH=src $PY -m pytest tests/test_json.py::test_json_decimal -q 2>&1 | grep -q "1 failed"; then
    echo "baseline OK: test_json_decimal fails (bug reproduced)"
else
    echo "baseline check failed: test_json_decimal did NOT fail" >&2
    exit 1
fi

echo "swe-flask4160 environment ready (ubuntu + standalone python3.10)"

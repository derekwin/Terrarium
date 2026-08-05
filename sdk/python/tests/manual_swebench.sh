#!/bin/bash
# Manual verification: a real SWE-bench instance as an agent execution
# environment (pallets__flask-4160 — Decimal JSON encoding).
#
# Proves the Agent-execution use case end-to-end:
#   terra tool create --script  bakes repo + deps + tests into a LAYER
#   (ready state; the bug reproduces at build time)
#   Batch of N envs from one snapshot: bug present -> agent applies the
#   fix -> tests pass -> reset_in_place restores the layer baseline.
#
# Requirements: real KVM host, `terra setup ubuntu`, sudo-started daemon
# (`TERRA_HOME=... python -m terra daemon start` — root runs the NAT
# bridge and mounts layers). Run as the regular user against that daemon.
#
# Build the environment layer once:
#   terra tool create -n swe-flask4160 --template ubuntu \
#       --script images/examples/swe-flask-4160-ubuntu.sh --timeout 1100

set -u
export HOME=${HOME:?} TERRA_HOME=${TERRA_HOME:?} PYTHONPATH=${PYTHONPATH:?}
cd "$(dirname "$0")/../.." || exit 1

timeout 420 python3 - <<"PY"
import base64, os, time
from terra.batch import Batch
from terra.client import TerraClient
from terra.sandbox import Sandbox
from terra._engine import DaemonManager

DaemonManager().ensure_running()
c = TerraClient()
layers = ["swe-flask4160", "ubuntu"]
snap = "/tmp/swe-agent-snap"

# ready snapshot of the SWE-bench environment (bug present)
Sandbox(tenant="swe-agent-src", layers=layers)
c.vm_snapshot("tenant-swe-agent-src", snap)
c.vm_destroy("tenant-swe-agent-src")
print("ready snapshot built", flush=True)

# the gold patch (agent's fix to src/flask/json/__init__.py), base64
gold = b"""diff --git a/src/flask/json/__init__.py b/src/flask/json/__init__.py
--- a/src/flask/json/__init__.py
+++ b/src/flask/json/__init__.py
@@ -1,3 +1,4 @@
+import decimal
 import io
 import json as _json
 import typing as t
@@ -47,7 +48,7 @@ def default(self, o: t.Any) -> t.Any:
         \"\"\"
         if isinstance(o, date):
             return http_date(o)
-        if isinstance(o, uuid.UUID):
+        if isinstance(o, (decimal.Decimal, uuid.UUID)):
             return str(o)
         if dataclasses and dataclasses.is_dataclass(o):
             return dataclasses.asdict(o)
"""
b64 = base64.b64encode(gold).decode()

with Batch(snap, 4, layers=layers, prefix="swa-") as envs:
    # 1) every agent starts against the broken baseline
    before = envs.collect(
        ["sh", "-c", "cp -r /opt/flask /workdir/flask && cd /workdir/flask && PYTHONPATH=src /opt/python310/bin/python3.10 -m pytest tests/test_json.py::test_json_decimal -q 2>&1 | tail -1"],
        timeout_secs=60,
    )
    broken = all("failed" in v for v in before.values())
    print(f"baseline broken across {len(envs)} envs: {broken}", flush=True)

    # 2) each agent applies the fix
    envs.exec(
        ["sh", "-c", f"echo {b64} | base64 -d > /tmp/fix.patch && cd /workdir/flask && git apply /tmp/fix.patch && rm /tmp/fix.patch"],
        timeout_secs=30,
    )
    fixed = envs.collect(
        ["sh", "-c", "cd /workdir/flask && PYTHONPATH=src /opt/python310/bin/python3.10 -m pytest tests/test_json.py -q 2>&1 | tail -1"],
        timeout_secs=120,
    )
    fixed_ok = all("passed" in v for v in fixed.values())
    print("fixed:", {k: v.strip() for k, v in fixed.items()}, flush=True)

    # 3) in-place reset -> back to the layer baseline (bug again)
    envs.reset_in_place()
    after = envs.collect(
        ["sh", "-c", "cp -r /opt/flask /workdir/flask && cd /workdir/flask && PYTHONPATH=src /opt/python310/bin/python3.10 -m pytest tests/test_json.py::test_json_decimal -q 2>&1 | tail -1"],
        timeout_secs=60,
    )
    reset_ok = all("failed" in v for v in after.values())
    print(f"reset restores baseline: {reset_ok}", flush=True)

    ok = broken and fixed_ok and reset_ok
    print("SWE-bench agent flow:", "OK" if ok else "FAIL")
    raise SystemExit(0 if ok else 1)
PY

#!/bin/bash
# Manual verification of the environment-layer toolchain (P1/RL):
#
#   terra tool create --script  bakes the environment READY state into a
#   LAYER (images/examples/rl-env.sh); episode writes go to the VM upper
#   and are cleared by Batch.reset_in_place() — the layer baseline must
#   survive every reset.
#
# Requirements: real KVM host, `terra setup alpine`, daemon privileges.
# Run inside the privileged verification container (unshare + /dev/kvm):
#
#   docker run --rm --privileged -v /home/<user>:/home/<user> \
#     -e HOME=/home/<user> -e TERRA_HOME=/home/<user>/.local/share/terra \
#     -e PYTHONPATH=/home/<user>/2606/Terrarium/sdk/python \
#     python:3.12-slim bash sdk/python/tests/manual_envlayer.sh

set -u
export HOME=${HOME:?} TERRA_HOME=${TERRA_HOME:?} PYTHONPATH=${PYTHONPATH:?}
cd /home/liujinyao/2606/Terrarium 2>/dev/null || cd "$(dirname "$0")/../../.."
export PATH=/home/liujinyao/.local/bin:/home/liujinyao/.local/share/terra/bin:/home/liujinyao/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

python3.12 - <<"PY" &
import time
from terra._engine import DaemonManager
DaemonManager().ensure_running()
time.sleep(600)
PY
KEEPER=$!
sleep 4

echo "=== build rl-env layer ==="
python3.12 -m terra tool create -n rl-env --template alpine --no-net --script images/examples/rl-env.sh 2>&1 | tail -2

echo "=== RL episode flow (layer-backed ready state) ==="
timeout 300 python3.12 - <<"PY"
from terra.batch import Batch
from terra.client import TerraClient
from terra.sandbox import Sandbox
from terra._engine import DaemonManager

DaemonManager().ensure_running()
c = TerraClient()
Sandbox(tenant="rl-src", layers=["rl-env"])
c.vm_snapshot("tenant-rl-src", "/tmp/rl-env-snap")
c.vm_destroy("tenant-rl-src")
print("snapshot built")

with Batch("/tmp/rl-env-snap", 4, layers=["rl-env"], prefix="rle-") as envs:
    before = envs.collect(["cat", "/var/rl-ready"], timeout_secs=30)
    print("ready state before:", {k: v.strip() for k, v in before.items()})
    for ep in range(3):
        envs.exec(["sh", "-c", "echo ep-state > /workdir/ep && echo tmp > /tmp/ep"])
        envs.reset_in_place()
        ready = envs.collect(["cat", "/var/rl-ready"], timeout_secs=30)
        cleared = envs.collect(
            ["sh", "-c", "test ! -e /workdir/ep && test ! -e /tmp/ep && echo CLEAN || echo DIRTY"],
            timeout_secs=30,
        )
        ready_ok = all(v.strip() == "ready" for v in ready.values())
        clear_ok = all(v.strip() == "CLEAN" for v in cleared.values())
        print(f"ep{ep}: ready={ready_ok} cleared={clear_ok}")
        if not (ready_ok and clear_ok):
            raise SystemExit(f"FAIL: ready={ready} cleared={cleared}")
print("env-layer toolchain OK")
PY
RC=$?
kill $KEEPER 2>/dev/null
exit $RC

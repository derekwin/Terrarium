#!/usr/bin/env python3
"""Minimal RL episode loop on the layer-baseline pipeline (real KVM).

This is the training-loop contract the engine is optimized for:

1. The environment READY state lives in a LAYER, built once with
   ``terra tool create -n rl-env --template alpine --no-net
   --script images/examples/rl-env.sh`` (layer = distro + ready-state +
   task machinery; episode writes go to the VM's writable upper).
2. Each episode: inject the episode input into every env
   (``/workdir/input.json``) -> run the LAYER task in parallel
   (``/usr/local/bin/rl-task``) -> collect results (stdout / output.json)
   -> ``Batch.reset_in_place()`` clears the upper back to the layer
   baseline. The ready state survives every reset by construction.

No training framework is required to read this: the same 4 calls
(exec / collect / reset_in_place, plus report for density) are what a
Torch/Ray/Gym loop would wrap.

Requirements: /dev/kvm, guest assets (``terra setup alpine``), daemon
privileges (run in the same privileged container as the other real-KVM
tests). The rl-env layer must exist first (build it with the tool
command above); the example snapshots its ready state on first run.

Run::

    python3 sdk/python/examples/rl_episode_loop.py --envs 8 --episodes 10
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if REPO not in sys.path:
    sys.path.insert(0, REPO)

from terra.batch import Batch  # noqa: E402
from terra.client import TerraClient  # noqa: E402
from terra.sandbox import Sandbox  # noqa: E402
from terra._engine import DaemonManager  # noqa: E402


def ensure_ready_snapshot(snapshot_path: str, layers: list[str]) -> None:
    """Boot a layered VM from the rl-env baseline and snapshot it."""
    DaemonManager().ensure_running()
    c = TerraClient()
    Sandbox(tenant="rl-loop-src", layers=layers)
    c.vm_snapshot("tenant-rl-loop-src", snapshot_path)
    c.vm_destroy("tenant-rl-loop-src")
    print(f"ready snapshot: {snapshot_path}")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--envs", type=int, default=8, help="parallel environments")
    ap.add_argument("--episodes", type=int, default=10, help="episodes to run")
    ap.add_argument("--layers", default="rl-env", help="comma-separated layers")
    ap.add_argument("--snapshot", default="/tmp/rl-episode-snap", help="snapshot path")
    args = ap.parse_args()

    layers = [s.strip() for s in args.layers.split(",") if s.strip()]
    if not os.path.isdir(args.snapshot):
        ensure_ready_snapshot(args.snapshot, layers)

    with Batch(args.snapshot, args.envs, layers=layers, prefix="rle-") as envs:
        for ep in range(args.episodes):
            t0 = time.perf_counter()

            # 1) inject this episode's input into every env (upper).
            payload = {"episode": ep, "x": 50 + ep}
            envs.exec(
                ["sh", "-c", f"echo '{json.dumps(payload)}' > /workdir/input.json"],
                timeout_secs=30,
            )

            # 2) run the layer task in parallel; collect stdout.
            outs = envs.collect(["/usr/local/bin/rl-task"], timeout_secs=30)

            # 3) consume results (the task also wrote /workdir/output.json).
            ok = all(v.strip().startswith("result-") for v in outs.values())
            sample = next(iter(outs.values())).strip()

            # 4) in-place reset back to the layer baseline.
            envs.reset_in_place()
            dt = (time.perf_counter() - t0) * 1000
            print(f"episode {ep:>3}: {dt:6.1f} ms  ok={ok}  sample={sample}", flush=True)
            if not ok:
                print("FAIL:", outs, file=sys.stderr)
                return 1

    print(f"\nRL episode loop OK: {args.episodes} episodes x {args.envs} envs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

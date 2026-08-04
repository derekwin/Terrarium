"""Manual real-KVM benchmark: RL episode loop on the fast-reset pipeline.

Models the primary-market cadence: N environments restored from ONE
snapshot, then repeated episodes of {task injection -> parallel run ->
collect results -> snapshot recycle (deterministic reset)}. Reports
episode wall time, reset share, and environment throughput.

REQUIRES: /dev/kvm, guest assets, daemon privileges (same privileged
container as the other real-KVM benchmarks).

Run::

    python3 sdk/python/tests/manual_episode_bench.py --sizes 4,8,16 --episodes 5
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from typing import Any

from terra.batch import Batch
from terra.client import TerraClient
from terra.sandbox import Sandbox
from terra._engine import DaemonManager


def build_ready_snapshot(path: str, layers: list[str]) -> str:
    """Boot a layered VM with a pristine task output, snapshot, destroy."""
    DaemonManager().ensure_running()
    c = TerraClient()
    Sandbox(tenant="ep-bench-src", layers=layers)
    vm = "tenant-ep-bench-src"
    r = c.vm_exec(vm, ["sh", "-c", "rm -f /var/task.out && echo ready > /var/state"])
    if r["exit_code"] != 0:
        raise RuntimeError(f"setup failed: {r}")
    c.vm_snapshot(vm, path)
    c.vm_destroy(vm)
    return path


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--snapshot", default=None, help="existing snapshot dir")
    ap.add_argument("--sizes", default="4,8,16", help="comma-separated batch sizes")
    ap.add_argument("--episodes", type=int, default=5, help="episodes per size")
    ap.add_argument("--layers", default="base")
    ap.add_argument("--task", default="inject", help="task mode: inject | compute")
    ap.add_argument("--out", default=None, help="write JSON to FILE")
    args = ap.parse_args()

    if not os.path.exists("/dev/kvm"):
        print("episode bench requires /dev/kvm", file=sys.stderr)
        return 2

    sizes = [int(s) for s in args.sizes.split(",") if s.strip()]
    layers = [s.strip() for s in args.layers.split(",") if s.strip()]

    snap = args.snapshot
    if snap is None:
        snap = "/tmp/episode-bench-snap"
        build_ready_snapshot(snap, layers)

    results: dict[str, Any] = {"snapshot": snap, "episodes": args.episodes, "task": args.task, "sizes": []}
    for n in sizes:
        with Batch(snap, n, layers=layers, prefix=f"ep-{n}-") as envs:
            episode_ms: list[float] = []
            inject_ms: list[float] = []
            collect_ms: list[float] = []
            reset_ms: list[float] = []
            all_ok = True
            for ep in range(args.episodes):
                t0 = time.perf_counter()
                if args.task == "inject":
                    # task injection: write the episode's input into each env
                    envs.exec(["sh", "-c", f"echo ep-{ep} > /var/task.in"])
                    inject = (time.perf_counter() - t0) * 1000
                    t0 = time.perf_counter()
                    res = envs.collect(["sh", "-c", "cat /var/task.in"], timeout_secs=30)
                    collect = (time.perf_counter() - t0) * 1000
                    ok = all(v.strip() == f"ep-{ep}" for v in res.values())
                else:
                    # compute task: bounded loop + result marker
                    t0 = time.perf_counter()
                    res = envs.collect(
                        ["sh", "-c", "i=0; while [ $i -lt 200 ]; do i=$((i+1)); done; echo $i"],
                        timeout_secs=30,
                    )
                    collect = (time.perf_counter() - t0) * 1000
                    inject = 0.0
                    ok = all(v.strip() == "200" for v in res.values())
                t0 = time.perf_counter()
                envs.recycle()
                reset = (time.perf_counter() - t0) * 1000

                episode = inject + collect + reset
                inject_ms.append(inject)
                collect_ms.append(collect)
                reset_ms.append(reset)
                episode_ms.append(episode)
                all_ok = all_ok and ok
                print(
                    f"size={n} ep={ep}: inject {inject:.0f} collect {collect:.0f} "
                    f"reset {reset:.0f} total {episode:.0f}ms ok={ok}",
                    flush=True,
                )

            total = sum(episode_ms)
            results["sizes"].append({
                "size": n,
                "episode_mean_ms": round(sum(episode_ms) / len(episode_ms), 1),
                "inject_mean_ms": round(sum(inject_ms) / len(inject_ms), 1),
                "collect_mean_ms": round(sum(collect_ms) / len(collect_ms), 1),
                "reset_mean_ms": round(sum(reset_ms) / len(reset_ms), 1),
                "reset_share_pct": round(
                    sum(reset_ms) / total * 100, 1
                ),
                "episodes_per_min": round(
                    args.episodes / (total / 1000) * 60, 1
                ),
                "envs_per_sec": round(
                    n * args.episodes / (total / 1000), 1
                ),
                "all_ok": all_ok,
            })

    doc = json.dumps(results, indent=2)
    if args.out:
        with open(args.out, "w") as f:
            f.write(doc + "\n")
        print(f"results written to {args.out}")
    else:
        print(doc)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

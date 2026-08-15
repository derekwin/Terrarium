"""Manual real-KVM benchmark: P1 fast-reset scaling (snapshot concurrency).

Measures how many environments can be restored from ONE snapshot in
parallel — batch restore/recycle wall time, per-env latency, host memory —
plus parallel collect throughput: the RL episode-scaling numbers.

REQUIRES: /dev/kvm, guest assets, daemon privileges (run in the same
privileged container as the other real-KVM benchmarks). The engine's VM
lifecycle runs lock-free (see docs/benchmarks.md fast-reset section), so
restores genuinely overlap.

Run::

    python3 sdk/python/tests/manual_reset_bench.py --sizes 4,8,16,32,64
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
    """Boot a layered VM, write a marker, snapshot it, destroy the VM."""
    DaemonManager().ensure_running()
    c = TerraClient()
    Sandbox(tenant="reset-bench-src", layers=layers)
    vm = "tenant-reset-bench-src"
    r = c.vm_exec(vm, ["sh", "-c", "echo ready > /var/marker"])
    if r["exit_code"] != 0:
        raise RuntimeError(f"marker write failed: {r}")
    c.vm_snapshot(vm, path)
    c.vm_destroy(vm)
    return path


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--snapshot", default=None, help="existing snapshot dir")
    ap.add_argument("--sizes", default="4,8,16,32,64", help="comma-separated batch sizes")
    ap.add_argument("--layers", default="base")
    ap.add_argument("--workers", type=int, default=32, help="client concurrency cap")
    ap.add_argument("--out", default=None, help="write JSON to FILE")
    args = ap.parse_args()

    if not os.path.exists("/dev/kvm"):
        print("reset bench requires /dev/kvm", file=sys.stderr)
        return 2

    sizes = [int(s) for s in args.sizes.split(",") if s.strip()]
    layers = [s.strip() for s in args.layers.split(",") if s.strip()]

    snap = args.snapshot
    snap_ms = None
    if snap is None:
        snap = "/tmp/reset-bench-snap"
        t0 = time.perf_counter()
        build_ready_snapshot(snap, layers)
        snap_ms = round((time.perf_counter() - t0) * 1000, 1)

    results: dict[str, Any] = {"snapshot_path": snap, "snapshot_ms": snap_ms, "sweeps": []}
    for n in sizes:
        workers = max(1, min(args.workers, n))
        t0 = time.perf_counter()
        with Batch(snap, n, layers=layers, prefix=f"rb-{n}-", workers=workers) as envs:
            restore_ms = (time.perf_counter() - t0) * 1000

            t0 = time.perf_counter()
            out = envs.collect(["cat", "/var/marker"], timeout_secs=30)
            collect_ms = (time.perf_counter() - t0) * 1000
            collect_ok = all(v.strip() == "ready" for v in out.values())

            rep = envs.report()
            t0 = time.perf_counter()
            envs.recycle()
            recycle_ms = (time.perf_counter() - t0) * 1000

            results["sweeps"].append({
                "size": n,
                "restore_ms": round(restore_ms, 1),
                "per_vm_ms": round(restore_ms / n, 1),
                "recycle_ms": round(recycle_ms, 1),
                "collect_ms": round(collect_ms, 1),
                "execs_per_sec": round(n / (collect_ms / 1000), 1),
                "collect_ok": collect_ok,
                "rss_mb": rep["rss_mb"],
                "pss_mb": rep["pss_mb"],
                "per_vm_rss_mb": round(rep["rss_mb"] / n, 1) if rep["rss_mb"] else None,
                "per_vm_pss_mb": round(rep["pss_mb"] / n, 1) if rep["pss_mb"] else None,
            })
            print(
                f"size={n}: restore {restore_ms:.0f}ms ({restore_ms/n:.0f}/vm) "
                f"recycle {recycle_ms:.0f}ms collect {collect_ms:.0f}ms "
                f"{n/(collect_ms/1000):.0f}/s ok={collect_ok}",
                flush=True,
            )

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

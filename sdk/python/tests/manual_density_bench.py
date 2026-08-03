"""Manual real-KVM density benchmark for Terrarium.

REQUIRES: /dev/kvm, guest assets (``terra setup <distro>``), and daemon
privileges. Not part of CI — CI has no KVM runner, and this script exits 2
when KVM is unavailable rather than pretending to measure something.

What it measures (methodology in docs/benchmarks.md):

1. ``cold_create_ms``  — :class:`~terra.sandbox.Sandbox` on a fresh tenant
   (VM boot included), for 1..N tenants.
2. ``per_vm_pss_mb``   — host memory per tenant VM from
   ``/proc/<pid>/smaps_rollup`` Pss (VmRSS fallback). Pss counts shared
   EROFS layer pages once across VMs, which is exactly the "shared
   page-cache layers" density claim; comparing Pss vs RSS shows the sharing.
3. ``sandbox_in_tenant_ms`` — extra ``Sandbox()`` calls in an existing
   tenant VM (density within a tenant).
4. ``warm_claim_ms`` / ``warm_exec_ms`` — ``Pool.acquire()`` claim latency
   and exec latency on a pre-booted pool VM.
5. ``exec_ms``         — blocking exec latency (p50/p95/mean).
6. ``execs_per_sec``   — concurrent exec throughput on one tenant VM.

Run::

    python3 sdk/python/tests/manual_density_bench.py --tenants 4

Output: a JSON document on stdout (or ``--out FILE``) plus a summary table.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import math
import os
import statistics
import sys
import time
from typing import Any

from terra.client import TerraClient
from terra.exceptions import TerraError
from terra.pool import Pool
from terra.sandbox import Sandbox


def host_mem_mb() -> int:
    pages = os.sysconf("SC_PHYS_PAGES")
    page_size = os.sysconf("SC_PAGE_SIZE")
    return (pages * page_size) // (1024 * 1024)


def vm_memory_mb(pid: int) -> tuple[float | None, float | None]:
    """Return (rss_mb, pss_mb) for a host process, or (None, None) if the
    procfs file is unreadable (e.g. the daemon runs as another user).
    Pss is read from smaps_rollup when available so shared layer pages are
    counted only once across VMs."""
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            rss = pss = None
            for line in f:
                if line.startswith("Rss:"):
                    rss = int(line.split()[1]) / 1024.0
                elif line.startswith("Pss:"):
                    pss = int(line.split()[1]) / 1024.0
            return rss, pss
    except (OSError, ValueError):
        try:
            with open(f"/proc/{pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        rss = int(line.split()[1]) / 1024.0
                        return rss, None
        except (OSError, ValueError):
            return None, None
        return None, None


def host_vm_memory_mb(client: TerraClient) -> tuple[float | None, float | None]:
    """Sum (rss, pss) over every registered VM's host process."""
    vms = client.vm_list().get("vms", [])
    rss_sum = pss_sum = 0.0
    seen_rss = seen_pss = False
    for vm in vms:
        pid = vm.get("pid")
        if not pid:
            continue
        rss, pss = vm_memory_mb(pid)
        if rss is not None:
            rss_sum += rss
            seen_rss = True
        if pss is not None:
            pss_sum += pss
            seen_pss = True
    # virtiofsd processes serve the composed layers; their footprint is
    # part of the per-VM host cost (the shared file cache itself lives in
    # the kernel page cache, shared across VMs, and is not per-process).
    for pid in _proc_comm_pids("virtiofsd"):
        rss, pss = vm_memory_mb(pid)
        if rss is not None:
            rss_sum += rss
            seen_rss = True
        if pss is not None:
            pss_sum += pss
            seen_pss = True
    return (rss_sum if seen_rss else None), (pss_sum if seen_pss else None)


def _proc_comm_pids(name: str) -> list[int]:
    """PIDs whose process comm equals *name* (best-effort /proc scan)."""
    out: list[int] = []
    try:
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            try:
                with open(f"/proc/{entry}/comm") as f:
                    if f.read().strip() == name:
                        out.append(int(entry))
            except OSError:
                continue
    except OSError:
        pass
    return out


def median_pct(latencies: list[float]) -> dict[str, float]:
    if not latencies:
        return {}
    latencies = sorted(latencies)
    n = len(latencies)

    def percentile(p: float) -> float:
        # Nearest-rank: ceil(p/100 * n)th smallest sample.
        return latencies[min(n, max(1, math.ceil(p / 100 * n))) - 1]

    return {
        "p50_ms": round(percentile(50), 2),
        "p95_ms": round(percentile(95), 2),
        "mean_ms": round(statistics.fmean(latencies), 2),
        "n": len(latencies),
    }


def bench_cold_create(
    client: TerraClient, tenants: int, layers: list[str], results: dict[str, Any]
) -> list[str]:
    created: list[str] = []
    latencies: list[float] = []
    memory_rows: list[dict[str, Any]] = []
    base_rss, base_pss = host_vm_memory_mb(client)
    prev_delta: tuple[float, float] | None = None
    for i in range(1, tenants + 1):
        tenant = f"bench-cold-{i}"
        t0 = time.perf_counter()
        Sandbox(tenant=tenant, layers=layers)
        latencies.append((time.perf_counter() - t0) * 1000)
        created.append(tenant)
        rss, pss = host_vm_memory_mb(client)
        drss = None if rss is None or base_rss is None else rss - base_rss
        dpss = None if pss is None or base_pss is None else pss - base_pss
        row: dict[str, Any] = {"tenants": i, "rss_mb": round(drss, 1) if drss is not None else None}
        if dpss is not None:
            row["pss_mb"] = round(dpss, 1)
            # Sharing quantification: RSS counts shared layer pages once per
            # VM, Pss counts them once across all VMs — the gap is the
            # page-cache sharing benefit.
            row["shared_mb"] = round(drss - dpss, 1)
            row["shared_pct"] = round((drss - dpss) / drss * 100, 1) if drss else 0.0
            row["per_vm_rss_mb"] = round(drss / i, 1)
            row["per_vm_pss_mb"] = round(dpss / i, 1)
            if prev_delta is not None:
                row["incr_rss_mb"] = round(drss - prev_delta[0], 1)
                row["incr_pss_mb"] = round(dpss - prev_delta[1], 1)
            prev_delta = (drss, dpss)
        memory_rows.append(row)
    results["cold_create_ms"] = median_pct(latencies)
    results["per_vm_memory_mb"] = memory_rows
    if memory_rows and memory_rows[-1].get("pss_mb") is not None:
        last = memory_rows[-1]
        results["sharing_summary"] = {
            "tenants": last["tenants"],
            "rss_mb": last["rss_mb"],
            "pss_mb": last["pss_mb"],
            "shared_mb": last["shared_mb"],
            "shared_pct": last["shared_pct"],
            "per_vm_rss_mb": last["per_vm_rss_mb"],
            "per_vm_pss_mb": last["per_vm_pss_mb"],
        }
    return created


def bench_sandboxes_in_tenant(
    tenant: str, count: int, results: dict[str, Any]
) -> None:
    latencies: list[float] = []
    for _ in range(count):
        t0 = time.perf_counter()
        Sandbox(tenant=tenant, layers=["base"])
        latencies.append((time.perf_counter() - t0) * 1000)
    results["sandbox_in_tenant_ms"] = median_pct(latencies)


def bench_pool(pool_size: int, results: dict[str, Any]) -> Sandbox:
    pool = Pool(layers=["base"], size=pool_size)
    t0 = time.perf_counter()
    sb = pool.acquire()
    results["warm_claim_ms"] = round((time.perf_counter() - t0) * 1000, 2)
    t0 = time.perf_counter()
    sb.exec(["echo", "ok"])
    results["warm_exec_ms"] = round((time.perf_counter() - t0) * 1000, 2)
    return sb


def bench_exec(sb: Sandbox, repeats: int, results: dict[str, Any]) -> None:
    latencies: list[float] = []
    for _ in range(repeats):
        for attempt in range(5):
            try:
                t0 = time.perf_counter()
                r = sb.exec(["echo", "ok"])
                break
            except TerraError:
                if attempt == 4:
                    raise
                time.sleep(0.2)
        if r.exit_code != 0:
            raise RuntimeError(f"exec failed: {r.stderr!r}")
        latencies.append((time.perf_counter() - t0) * 1000)
    results["exec_ms"] = median_pct(latencies)


def bench_throughput(
    sb: Sandbox, concurrency: int, total_execs: int, results: dict[str, Any]
) -> None:
    per_worker = max(1, total_execs // concurrency)

    def worker() -> None:
        for _ in range(per_worker):
            for attempt in range(5):
                try:
                    r = sb.exec(["echo", "ok"])
                    break
                except TerraError:
                    if attempt == 4:
                        raise
                    time.sleep(0.2)
            if r.exit_code != 0:
                raise RuntimeError(f"exec failed: {r.stderr!r}")

    t0 = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as ex:
        list(ex.map(lambda _: worker(), range(concurrency)))
    wall = time.perf_counter() - t0
    results["throughput"] = {
        "concurrency": concurrency,
        "execs": concurrency * per_worker,
        "wall_s": round(wall, 2),
        "execs_per_sec": round((concurrency * per_worker) / wall, 1),
    }


def cleanup(client: TerraClient, tenants: list[str]) -> None:
    for tenant in tenants:
        try:
            client.tenant_destroy(tenant)
        except Exception:  # noqa: BLE001 - best-effort cleanup
            pass
    try:
        vms = client.vm_list().get("vms", [])
    except Exception as e:  # noqa: BLE001 - cleanup must never mask the result
        print(f"cleanup: vm_list failed ({e}); pool VMs may linger", file=sys.stderr)
        vms = []
    for vm in vms:
        if vm.get("name", "").startswith("pool-"):
            try:
                client.vm_destroy(vm["name"])
            except Exception:  # noqa: BLE001
                pass


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--tenants", type=int, default=4, help="cold-create sweep size")
    ap.add_argument("--sandboxes-per-tenant", type=int, default=8)
    ap.add_argument(
        "--layers",
        type=str,
        default="base",
        help="comma-separated layer list for the sweep (default: base)",
    )
    ap.add_argument("--label", type=str, default=None, help="result label (host section)")
    ap.add_argument("--pool-size", type=int, default=2)
    ap.add_argument(
        "--concurrency",
        type=int,
        default=16,
        help="concurrent execs for throughput (a 1-vCPU guest-proxy drops "
        "connections above ~16-32 concurrent execs; see docs/benchmarks.md)",
    )
    ap.add_argument("--total-execs", type=int, default=64)
    ap.add_argument("--repeats", type=int, default=10, help="exec latency repeats")
    ap.add_argument("--out", type=str, default=None, help="write JSON to FILE")
    args = ap.parse_args()

    if not os.path.exists("/dev/kvm"):
        print("density bench requires /dev/kvm — not measuring anything.", file=sys.stderr)
        return 2

    client = TerraClient()
    results: dict[str, Any] = {
        "host": {
            "kvm": True,
            "mem_total_mb": host_mem_mb(),
            "argv": sys.argv[1:],
        },
    }
    layers = [s.strip() for s in args.layers.split(",") if s.strip()]
    if args.label:
        results["host"]["label"] = args.label
    created: list[str] = []
    try:
        # Warm-up: asset resolution + daemon readiness land on a throwaway
        # tenant, not on the measured sweep.
        Sandbox(tenant="bench-warmup", layers=layers)
        created.append("bench-warmup")

        created += bench_cold_create(client, args.tenants, layers, results)
        bench_sandboxes_in_tenant(
            created[-1], args.sandboxes_per_tenant, results
        )
        pool_sb = bench_pool(args.pool_size, results)
        bench_exec(pool_sb, args.repeats, results)
        bench_exec(Sandbox(tenant=created[-1], layers=["base"]), args.repeats, results)
        bench_throughput(
            Sandbox(tenant=created[-1], layers=["base"]),
            args.concurrency,
            args.total_execs,
            results,
        )
    except TerraError as e:
        # The environment reached the engine but cannot complete a sandbox
        # (missing guest assets, no mount/namespace privileges, ...). Honest
        # failure: report and clean up, never emit partial numbers.
        print(f"benchmark aborted: engine error — {e}", file=sys.stderr)
        print(
            "check guest assets (terra setup) and host privileges "
            "(virtiofsd needs user/mount namespaces)",
            file=sys.stderr,
        )
        return 3
    finally:
        cleanup(client, created)

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

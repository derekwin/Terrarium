#!/usr/bin/env python3
"""Governance / isolation overhead on REAL agent-style workloads.

Same static C workload probe runs identically in every environment —
host (bare), Terrarium VM unsandboxed (vm), Terrarium VM + confine
(vm+confine, the product path), docker, gVisor. No shell/interpreter
variance.

Decomposition:
  governance overhead = (vm+confine) / (vm)   — same VM, only L2 differs
  isolation overhead   = (vm+confine) / (bare)

Workloads (see workload_probe.c): fileio (create/read/unlink), subproc
(fork+exec), cpu (integer loop), mixed (agent-like write/build/parse).

Usage:
    python3 sdk/python/tests/workload_overhead.py --repeats 5 --out /tmp/overhead.json
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path


REPO = Path(__file__).resolve().parents[3]
PROBES = Path(__file__).resolve().parent / "adversarial" / "probes"
PROBE = PROBES / "workload_probe"
DOCKER_IMAGE = os.environ.get("TERRA_BASELINE_IMAGE", "python:3.12-slim")
RUNSC = str(Path.home() / ".local/bin/runsc")

# (mode, count) sized for roughly 1-5s per run.
WORKLOADS = {
    "fileio": ("fileio", 1500),
    "subproc": ("subproc", 150),
    "cpu": ("cpu", 3_000_000),
    "mixed": ("mixed", 10),
}


def _sh(cmd: list[str], timeout: int = 300, **kw) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, **kw)


def _sudo(cmd: list[str], timeout: int = 300) -> subprocess.CompletedProcess:
    n = _sh(["sudo", "-n", *cmd], timeout=timeout)
    if n.returncode == 0 or not os.environ.get("SUDO_PASSWORD"):
        return n
    return subprocess.run(
        ["sudo", "-S", *cmd],
        input=os.environ["SUDO_PASSWORD"] + "\n",
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def _build_probe() -> bool:
    if PROBE.exists():
        return True
    gcc = shutil.which("gcc")
    if gcc is None:
        return False
    return _sh([gcc, "-static", "-O2", "-o", str(PROBE), str(PROBES / "workload_probe.c")]).returncode == 0


def _upload(sb, chunk: int = 60000) -> None:
    data = base64.b64encode(PROBE.read_bytes()).decode()
    sb.exec(["sh", "-c", "rm -f /tmp/wp.b64"], sandboxed=False)
    for i in range(0, len(data), chunk):
        sb.exec(["sh", "-c", f"echo {data[i:i+chunk]} >> /tmp/wp.b64"], sandboxed=False)
    sb.exec(
        ["sh", "-c", "cat /tmp/wp.b64 | base64 -d > /tmp/workload_probe && chmod +x /tmp/workload_probe"],
        sandboxed=False,
    )


def _run_once(env: str, mode: str, count: int, scratch: str, sb) -> float:
    t0 = time.perf_counter()
    if env == "bare":
        r = _sh([str(PROBE), mode, scratch, str(count)])
        assert f"done-{mode}" in r.stdout, r
    elif env in ("vm", "vm+confine"):
        r = sb.exec(
            ["/tmp/workload_probe", mode, scratch, str(count)],
            sandboxed=(env == "vm+confine"),
            timeout=240,
        )
        assert r.exit_code == 0 and f"done-{mode}" in r.stdout, r
    elif env == "docker":
        r = _sh(["docker", "exec", "wload-cmp", "/probe", mode, scratch, str(count)])
        assert f"done-{mode}" in r.stdout, r
    elif env == "gvisor":
        r = _sudo([
            RUNSC, "--network=none", "--root", "/tmp/runsc-workload", "do", "--",
            str(PROBE), mode, scratch, str(count),
        ])
        assert f"done-{mode}" in r.stdout, r
    return (time.perf_counter() - t0) * 1000


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repeats", type=int, default=5)
    ap.add_argument("--workloads", default=",".join(WORKLOADS))
    ap.add_argument("--envs", default="bare,vm,vm+confine,docker,gvisor")
    ap.add_argument("--out", default="/tmp/workload-overhead.json")
    args = ap.parse_args()

    if not _build_probe():
        print("cannot build workload probe (gcc static required)", file=sys.stderr)
        return 1

    sys.path.insert(0, str(REPO / "sdk" / "python"))
    from terra.sandbox import Sandbox  # noqa: PLC0415

    envs = [e for e in args.envs.split(",") if e]
    workloads = [w for w in args.workloads.split(",") if w in WORKLOADS]
    print("envs:", envs, "| workloads:", workloads, flush=True)

    sb = None
    docker_ctr = None
    results: dict = {}
    try:
        if "docker" in envs:
            _sh(["docker", "rm", "-f", "wload-cmp"])
            docker_ctr = "wload-cmp"
            r = _sh([
                "docker", "run", "-d", "--name", docker_ctr,
                "-v", f"{PROBE}:/probe:ro",
                DOCKER_IMAGE, "sleep", "3600",
            ])
            assert r.returncode == 0, r
        if any(e in ("vm", "vm+confine") for e in envs):
            sb = Sandbox(tenant="wload", layers=["ubuntu"], network=False, timeout=600)
            _upload(sb)
        for name in workloads:
            mode, count = WORKLOADS[name]
            row: dict = {}
            for env in envs:
                samples: list[float] = []
                scratch = "/tmp/wload"
                for _ in range(args.repeats):
                    samples.append(_run_once(env, mode, count, scratch, sb))
                med = statistics.median(samples)
                row[env] = {
                    "median_ms": round(med, 1),
                    "samples_ms": [round(s, 1) for s in samples],
                }
                print(f"  {name:8s} {env:10s} median {med:9.1f} ms", flush=True)
            if "vm" in row and "vm+confine" in row and row["vm"]["median_ms"]:
                row["governance_overhead_x"] = round(
                    row["vm+confine"]["median_ms"] / row["vm"]["median_ms"], 3
                )
            if "bare" in row and "vm+confine" in row and row["bare"]["median_ms"]:
                row["isolation_overhead_x"] = round(
                    row["vm+confine"]["median_ms"] / row["bare"]["median_ms"], 3
                )
            results[name] = row
    finally:
        if sb is not None:
            sb.kill()
        if docker_ctr is not None:
            _sh(["docker", "rm", "-f", docker_ctr])

    Path(args.out).write_text(json.dumps(results, indent=2))
    print("wrote:", args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

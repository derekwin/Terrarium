#!/usr/bin/env python3
"""Cross-baseline density sweep: same N long-lived instances on one host.

Measures, for each of terra / docker / gvisor:

* creation throughput — wall time to reach N running instances
  (instances/sec);
* host memory curve — sum of the baseline's host processes at each step
  (terra: CH + virtiofsd; docker: container usage via ``docker stats``;
  gvisor: runsc sandbox processes). All numbers are host-side RSS unless
  noted; terra Pss is reported when smaps_rollup is readable (root);
* aggregate exec throughput — parallel blocking execs across N instances
  (terraform execs; docker exec; gVisor has no exec-over-running-sandbox
  via ``runsc do``, so it is skipped);
* per-instance incremental cost — (mem at N − mem at N/2) / (N/2).

Baselines missing on the host are skipped. The sweep cleans up every
instance it created, on every exit path.

Usage:
    python3 sdk/python/tests/density_compare.py --instances 100 --out /tmp/density.json

    # sudo password for runsc (no NOPASSWD):
    SUDO_PASSWORD=... python3 sdk/python/tests/density_compare.py
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from uuid import uuid4


REPO = Path(__file__).resolve().parents[3]
DOCKER_IMAGE = os.environ.get("TERRA_BASELINE_IMAGE", "python:3.12-slim")
RUNSC = str(Path.home() / ".local/bin/runsc")


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


def _proc_rss_mb(pid: int) -> float | None:
    r = _sh(["ps", "-o", "rss=", "-p", str(pid)])
    try:
        return float(r.stdout.strip()) / 1024
    except ValueError:
        return None


def _proc_pss_mb(pid: int) -> float | None:
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    return float(line.split()[1]) / 1024
    except (OSError, ValueError):
        return None
    return None


def _scan_proc_pids(needle: str) -> list[int]:
    out: list[int] = []
    for p in Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        try:
            cmdline = (p / "cmdline").read_bytes().decode(errors="ignore")
        except OSError:
            continue
        if needle in cmdline:
            out.append(int(p.name))
    return out


class DensityBaseline:
    name = ""

    def create(self, n: int, parallel: int) -> list[str]:
        raise NotImplementedError

    def host_memory_mb(self) -> dict:
        raise NotImplementedError

    def exec_throughput(self, handles: list[str], rounds: int, concurrency: int) -> float | None:
        raise NotImplementedError

    def cleanup(self, handles: list[str]) -> None:
        raise NotImplementedError


class TerraDensity(DensityBaseline):
    name = "terra"

    def __init__(self) -> None:
        sys.path.insert(0, str(REPO / "sdk" / "python"))
        from terra.client import TerraClient
        from terra.sandbox import Sandbox

        self.client = TerraClient()
        self.Sandbox = Sandbox
        self.prefix = f"dens-{uuid4().hex[:6]}"

    def create(self, n: int, parallel: int) -> list[str]:
        tenants = [f"{self.prefix}-{i}" for i in range(n)]

        def one(tenant: str) -> str:
            sb = self.Sandbox(tenant=tenant, layers=["ubuntu"], network=True, timeout=120)
            sb.kill()  # session gone; the tenant VM stays up
            return tenant

        if parallel > 1:
            with concurrent.futures.ThreadPoolExecutor(max_workers=parallel) as ex:
                list(ex.map(one, tenants))
        else:
            for t in tenants:
                one(t)
        return tenants

    def host_memory_mb(self) -> dict:
        vms = {v["name"]: v.get("pid") for v in self.client.vm_list().get("vms", [])}
        rss = pss = 0.0
        seen = False
        for name, pid in vms.items():
            if not name.startswith(f"tenant-{self.prefix}") or not pid:
                continue
            r = _proc_rss_mb(pid)
            p = _proc_pss_mb(pid)
            if r is not None:
                rss += r
                seen = True
            if p is not None:
                pss += p
            # the virtiofsd serving this tenant's composed layers (the CH
            # cmdline also contains the fs socket path, so match comm to
            # avoid double-counting the CH process)
            for vpid in _scan_proc_pids(f"terra-{name}-fs.sock"):
                try:
                    comm = Path(f"/proc/{vpid}/comm").read_text().strip()
                except OSError:
                    continue
                if comm != "virtiofsd":
                    continue
                r = _proc_rss_mb(vpid)
                p = _proc_pss_mb(vpid)
                if r is not None:
                    rss += r
                if p is not None:
                    pss += p
        return {"rss_mb": round(rss, 1) if seen else None,
                "pss_mb": round(pss, 1) if pss else None}

    def exec_throughput(self, handles: list[str], rounds: int, concurrency: int) -> float | None:
        import concurrent.futures as cf

        sandboxes = [self.Sandbox(tenant=t, layers=["ubuntu"], network=True, timeout=60) for t in handles]
        try:
            t0 = time.perf_counter()
            total = 0
            for _ in range(rounds):
                with cf.ThreadPoolExecutor(max_workers=concurrency) as ex:
                    for sb, r in zip(sandboxes, ex.map(lambda s: s.exec(["/bin/true"], sandboxed=True, timeout=30), sandboxes)):
                        assert r.exit_code == 0, r
                        total += 1
            return total / (time.perf_counter() - t0)
        finally:
            for sb in sandboxes:
                try:
                    sb.kill()
                except Exception:
                    pass

    def cleanup(self, handles: list[str]) -> None:
        from terra.exceptions import TerraError

        for t in handles:
            try:
                self.client.vm_destroy(f"tenant-{t}")
            except (TerraError, Exception):
                pass


class DockerDensity(DensityBaseline):
    name = "docker"

    def __init__(self) -> None:
        self.prefix = f"dens-{uuid4().hex[:6]}"

    def create(self, n: int, parallel: int) -> list[str]:
        names = [f"{self.prefix}-{i}" for i in range(n)]
        cmd = ["docker", "run", "-d", "--name", "PLACEHOLDER", DOCKER_IMAGE, "sleep", "3600"]
        for name in names:
            r = _sh([c if c != "PLACEHOLDER" else name for c in cmd])
            if r.returncode != 0:
                raise RuntimeError(f"docker run {name}: {r.stderr}")
        return names

    def host_memory_mb(self) -> dict:
        r = _sh(["docker", "stats", "--no-stream", "--format", "{{.Name}} {{.MemUsage}}"])
        total = 0.0
        count = 0
        for line in r.stdout.splitlines():
            parts = line.split()
            if len(parts) < 2 or not parts[0].startswith(self.prefix):
                continue
            import re

            m = re.match(r"([\d.]+)\s*([KMGT]?i?B)", parts[1])
            if not m:
                continue
            v = float(m.group(1))
            u = m.group(2)
            if u == "GiB":
                v *= 1024
            elif u == "KiB":
                v /= 1024
            total += v
            count += 1
        return {"rss_mb": round(total, 1) if count else None, "pss_mb": None}

    def exec_throughput(self, handles: list[str], rounds: int, concurrency: int) -> float | None:
        import concurrent.futures as cf

        def one(name: str) -> bool:
            return _sh(["docker", "exec", name, "/bin/true"]).returncode == 0

        t0 = time.perf_counter()
        total = 0
        for _ in range(rounds):
            with cf.ThreadPoolExecutor(max_workers=concurrency) as ex:
                for ok in ex.map(one, handles):
                    assert ok
                    total += 1
        return total / (time.perf_counter() - t0)

    def cleanup(self, handles: list[str]) -> None:
        for h in handles:
            _sh(["docker", "rm", "-f", h], timeout=120)


class GvisorDensity(DensityBaseline):
    name = "gvisor"

    def __init__(self) -> None:
        self.root = f"/tmp/runsc-dens-{uuid4().hex[:6]}"
        self.prefix = self.root
        self._procs: list[subprocess.Popen] = []

    def create(self, n: int, parallel: int) -> list[str]:
        # launch N long-lived `runsc do -- sleep` sandboxes concurrently
        pw = os.environ.get("SUDO_PASSWORD")
        sudo_base = ["sudo", "-n"] if not pw else ["sudo", "-S"]
        cmd = [*sudo_base, RUNSC, "--network=none", "--root", self.root, "do", "--",
               "sleep", "3600"]
        self._procs = []
        for _ in range(n):
            p = subprocess.Popen(
                cmd,
                stdin=subprocess.PIPE if pw else None,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            if pw:
                assert p.stdin is not None
                p.stdin.write((pw + "\n").encode())
                p.stdin.flush()
            self._procs.append(p)
        deadline = time.time() + 120
        while time.time() < deadline:
            sentries = _sh(["pgrep", "-f", "runsc-sandbox"])
            if len(sentries.stdout.split()) >= n:
                break
            time.sleep(0.5)
        alive = [p for p in self._procs if p.poll() is None]
        sentries = _sh(["pgrep", "-f", "runsc-sandbox"])
        if len(alive) < n or len(sentries.stdout.split()) < n:
            raise RuntimeError(
                f"gvisor: {len(alive)}/{n} wrappers, "
                f"{len(sentries.stdout.split())}/{n} sentries alive"
            )
        return [self.root] * n

    def host_memory_mb(self) -> dict:
        total = 0.0
        seen = False
        # runsc (do wrapper) + runsc-gofer + runsc-sandbox (sentry) all
        # carry "--network=none" in their cmdline (unique to gVisor
        # sandboxes here); sum the whole sandbox tree.
        for pid in _scan_proc_pids("--network=none"):
            r = _proc_rss_mb(pid)
            if r is not None:
                total += r
                seen = True
        return {"rss_mb": round(total, 1) if seen else None, "pss_mb": None}

    def exec_throughput(self, handles: list[str], rounds: int, concurrency: int) -> float | None:
        return None  # runsc do has no exec-over-running-sandbox shortcut

    def cleanup(self, handles: list[str]) -> None:
        for p in self._procs:
            p.terminate()
        for p in self._procs:
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
        # cascade to gofer/sentry children that may outlive the wrapper
        for pid in _scan_proc_pids(self.root) + _scan_proc_pids("runsc-do"):
            try:
                os.kill(pid, 9)
            except (ProcessLookupError, PermissionError):
                pass
        _sudo(["/bin/sh", "-c", f"rm -rf {self.root}"])


def sweep(base: DensityBaseline, n: int, parallel: int) -> dict:
    t0 = time.perf_counter()
    handles = base.create(n, parallel)
    create_s = time.perf_counter() - t0
    mem_n = base.host_memory_mb()
    exec_ps = base.exec_throughput(handles, rounds=3, concurrency=min(16, n))
    result = {
        "baseline": base.name,
        "instances": n,
        "create_wall_sec": round(create_s, 2),
        "instances_per_sec": round(n / create_s, 2) if create_s else None,
        "host_memory_mb": mem_n,
        "per_instance_mb": round(mem_n["rss_mb"] / n, 1) if mem_n.get("rss_mb") else None,
        "execs_per_sec": round(exec_ps, 1) if exec_ps else None,
    }
    base.cleanup(handles)
    return result


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--instances", type=int, default=100)
    ap.add_argument("--parallel", type=int, default=8)
    ap.add_argument("--baselines", default="terra,docker,gvisor")
    ap.add_argument("--out", default="/tmp/density-compare.json")
    args = ap.parse_args()

    classes: dict[str, type[DensityBaseline]] = {
        "terra": TerraDensity,
        "docker": DockerDensity,
        "gvisor": GvisorDensity,
    }
    results: dict = {"instances": args.instances, "baselines": {}}
    for name in args.baselines.split(","):
        cls = classes.get(name)
        if cls is None:
            continue
        if name == "docker" and shutil.which("docker") is None:
            print("skip docker: not installed")
            continue
        if name == "gvisor" and not Path(RUNSC).exists():
            print("skip gvisor: runsc missing")
            continue
        print(f"=== {name}: creating {args.instances} instances ===", flush=True)
        base = cls()
        try:
            row = sweep(base, args.instances, args.parallel)
        except Exception as e:  # noqa: BLE001
            print(f"  {name} failed: {e}")
            base.cleanup(getattr(base, "handles", []))
            row = {"baseline": name, "error": str(e)[:300]}
        results["baselines"][name] = row
        print(" ", row)

    Path(args.out).write_text(json.dumps(results, indent=2))
    print("wrote:", args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

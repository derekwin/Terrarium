#!/usr/bin/env python3
"""Cross-baseline comparison: same adversarial workload on every sandbox.

Runs a fixed workload matrix against:

* ``bare``           — the host process model (no confinement)
* ``docker-default`` — docker run (default caps, bridge network)
* ``docker-hardened``— docker run --cap-drop ALL --no-new-privileges
                       --read-only --network none
* ``gvisor``         — runsc do (--network=none, host fs readonly + overlay)
* ``terra``          — Terrarium VM + terra-confine default policy

Every baseline runs the SAME commands; the harness records the exact
outcome (exit code, stdout/stderr tail) plus host-side cost numbers
(cold-start latency, steady exec latency, per-instance memory). It does
not judge "safe vs unsafe" — the interpretation lives in
``docs/security-adversarial.md`` (e.g. docker/gVisor allow a container-local
write to /etc/passwd, Terrarium denies the shared-layer write; both are
meaningful but different guarantees).

Baselines present on the host are run; missing ones are reported as
``skipped`` (gVisor needs ``runsc`` + root, docker needs the daemon).

Usage:
    python3 sdk/python/tests/compare_baselines.py --out /tmp/baselines.json

    # when sudo needs a password (no NOPASSWD entry for runsc):
    SUDO_PASSWORD=... python3 sdk/python/tests/compare_baselines.py
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path
from uuid import uuid4


REPO = Path(__file__).resolve().parents[3]
PROBES_DIR = Path(__file__).resolve().parent / "adversarial" / "probes"
PROBE_BIN = PROBES_DIR / "escape_probe"
DOCKER_IMAGE = os.environ.get("TERRA_BASELINE_IMAGE", "python:3.12-slim")
LAN_HOST = "10.102.0.254"
LAN_PORT = 80


def _sh(cmd: list[str], timeout: int = 120, **kw) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, **kw)


def _sudo(cmd: list[str], timeout: int = 180) -> subprocess.CompletedProcess:
    """Run a root command via sudo (NOPASSWD preferred, SUDO_PASSWORD fallback)."""
    n = _sh(["sudo", "-n", *cmd], timeout=timeout)
    if n.returncode == 0 or not os.environ.get("SUDO_PASSWORD"):
        return n
    p = os.environ["SUDO_PASSWORD"]
    return subprocess.run(
        ["sudo", "-S", *cmd],
        input=p + "\n",
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def _available() -> dict[str, bool]:
    return {
        "bare": True,
        "docker-default": shutil.which("docker") is not None,
        "docker-hardened": shutil.which("docker") is not None,
        "gvisor": (Path.home() / ".local/bin/runsc").exists()
        or shutil.which("runsc") is not None,
        "terra": True,
    }


# ── workload matrix ────────────────────────────────────────────────────
WORKLOADS: list[tuple[str, list[str]]] = [
    ("fs_write_system", ["sh", "-c", "echo pwned >> /etc/passwd"]),
    ("fs_write_root", ["sh", "-c", "echo x > /root/x"]),
    ("fs_read_shadow", ["sh", "-c", "head -1 /etc/shadow 2>&1"]),
    ("device_devmem", ["sh", "-c", "dd if=/dev/mem bs=1 count=4 skip=4194304 2>&1"]),
    ("proc_kcore", ["sh", "-c", "head -c 4 /proc/kcore 2>&1"]),
    ("net_egress_tcp", ["/escape_probe", "net", "tcp", LAN_HOST, str(LAN_PORT)]),
    ("net_egress_udp", ["/escape_probe", "net", "udp", LAN_HOST, str(LAN_PORT)]),
    ("net_egress_raw", ["/escape_probe", "net", "raw", LAN_HOST]),
    ("caps", ["sh", "-c", "grep CapEff /proc/self/status 2>&1"]),
    ("pid_visibility", ["sh", "-c", "ls /proc | grep -c '^[0-9]' 2>&1"]),
]


class TerraRunner:
    def __init__(self) -> None:
        sys.path.insert(0, str(REPO / "sdk" / "python"))
        from terra.sandbox import Sandbox  # noqa: F401

        self.Sandbox = Sandbox
        self._sb: object | None = None
        self._tenant: str | None = None

    @property
    def sb(self):
        if self._sb is None:
            self._tenant = f"basecmp-{uuid4().hex[:8]}"
            self._sb = self.Sandbox(
                tenant=self._tenant,
                layers=["ubuntu"],
                network=True,
                timeout=300,
            )
            self._upload(self._sb, PROBE_BIN, "/tmp/escape_probe")
        return self._sb

    def _upload(self, sb, local: Path, remote: str, chunk: int = 60000) -> None:
        data = base64.b64encode(local.read_bytes()).decode()
        sb.exec(["sh", "-c", f"rm -f {remote}.b64"], sandboxed=False)
        for i in range(0, len(data), chunk):
            sb.exec(["sh", "-c", f"echo {data[i:i+chunk]} >> {remote}.b64"], sandboxed=False)
        sb.exec(
            ["sh", "-c", f"cat {remote}.b64 | base64 -d > {remote} && chmod +x {remote}"],
            sandboxed=False,
        )

    def run(self, argv: list[str]) -> dict:
        args = ["/tmp/escape_probe", *argv[1:]] if argv[0] == "/escape_probe" else argv
        r = self.sb.exec(args, sandboxed=True, timeout=60)
        return {"rc": r.exit_code, "stdout": r.stdout, "stderr": r.stderr}

    def close(self) -> None:
        if self._sb is not None:
            try:
                self._sb.kill()
            except Exception:
                pass
        if self._tenant is not None:
            try:
                self.Sandbox.destroy_tenant(self._tenant)
            except Exception:
                pass


def run_workload(baseline: str, argv: list[str], terra: TerraRunner) -> dict:
    if baseline == "bare":
        cmd = ["/bin/true"]
        if argv[0] == "/escape_probe":
            cmd = [str(PROBE_BIN), *argv[1:]]
        elif argv[0] == "sh":
            cmd = argv
        r = _sh(cmd)
        return {"rc": r.returncode, "stdout": r.stdout, "stderr": r.stderr}
    if baseline.startswith("docker"):
        cmd = [
            "docker", "run", "--rm",
            "-v", f"{PROBE_BIN}:/escape_probe:ro",
        ]
        if baseline == "docker-hardened":
            cmd += ["--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                    "--read-only", "--network", "none"]
        r = _sh([*cmd, DOCKER_IMAGE, *argv])
        return {"rc": r.returncode, "stdout": r.stdout, "stderr": r.stderr}
    if baseline == "gvisor":
        runsc = str(Path.home() / ".local/bin/runsc")
        if not Path(runsc).exists():
            runsc = shutil.which("runsc") or "runsc"
        cmd = ["/bin/true"]
        if argv[0] == "/escape_probe":
            cmd = [str(PROBE_BIN), *argv[1:]]
        elif argv[0] == "sh":
            cmd = argv
        r = _sudo([
            runsc, "--network=none", "--root", "/tmp/runsc-baseline", "do", "--",
            *cmd,
        ])
        return {"rc": r.returncode, "stdout": r.stdout, "stderr": r.stderr}
    if baseline == "terra":
        return terra.run(argv)
    raise AssertionError(baseline)


# ── perf ───────────────────────────────────────────────────────────────
def _cold_start_ms(baseline: str, terra: TerraRunner) -> float | None:
    t0 = time.perf_counter()
    if baseline == "bare":
        _sh(["/bin/true"])
    elif baseline.startswith("docker"):
        _sh(["docker", "run", "--rm", DOCKER_IMAGE, "/bin/true"])
    elif baseline == "gvisor":
        _sudo([str(Path.home() / ".local/bin/runsc"),
               "--network=none", "--root", "/tmp/runsc-baseline", "do", "--", "/bin/true"])
    elif baseline == "terra":
        # a fresh tenant: VM boot + first confined exec (no probe upload)
        from terra.sandbox import Sandbox

        tenant = f"cold-{uuid4().hex[:8]}"
        sb = Sandbox(tenant=tenant, layers=["ubuntu"], network=True, timeout=120)
        sb.exec(["/bin/true"], sandboxed=True, timeout=120)
        sb.kill()
        Sandbox.destroy_tenant(tenant)
        return (time.perf_counter() - t0) * 1000
    return (time.perf_counter() - t0) * 1000


def _exec_latency_ms(baseline: str, terra: TerraRunner, n: int = 30) -> float | None:
    if baseline == "bare":
        t0 = time.perf_counter()
        for _ in range(n):
            _sh(["/bin/true"])
        return (time.perf_counter() - t0) * 1000 / n
    if baseline.startswith("docker"):
        name = f"cmp-{uuid4().hex[:6]}"
        try:
            _sh(["docker", "run", "-d", "--name", name, DOCKER_IMAGE, "sleep", "3600"])
            t0 = time.perf_counter()
            for _ in range(n):
                _sh(["docker", "exec", name, "/bin/true"])
            return (time.perf_counter() - t0) * 1000 / n
        finally:
            _sh(["docker", "rm", "-f", name])
    if baseline == "gvisor":
        return None  # runsc do has no exec-over-running-sandbox shortcut
    if baseline == "terra":
        t0 = time.perf_counter()
        for _ in range(n):
            terra.sb.exec(["/bin/true"], sandboxed=True, timeout=60)
        return (time.perf_counter() - t0) * 1000 / n
    return None


def _memory_mb(baseline: str) -> float | None:
    if baseline.startswith("docker"):
        name = f"cmp-{uuid4().hex[:6]}"
        try:
            _sh(["docker", "run", "-d", "--name", name, DOCKER_IMAGE, "sleep", "3600"])
            time.sleep(0.5)
            r = _sh(["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", name])
            part = r.stdout.strip().split("/")[0].strip()
            m = re.match(r"([\d.]+)\s*([KMGT]?i?B)", part)
            if not m:
                return None
            val = float(m.group(1))
            unit = m.group(2)
            if unit == "GiB":
                return val * 1024
            if unit == "KiB":
                return val / 1024
            return val  # MiB
        finally:
            _sh(["docker", "rm", "-f", name])
    if baseline == "gvisor":
        runsc = str(Path.home() / ".local/bin/runsc")
        pw = os.environ.get("SUDO_PASSWORD")
        sudo_cmd = ["sudo", "-n"] if not pw else ["sudo", "-S"]
        p = subprocess.Popen(
            [*sudo_cmd, runsc, "--network=none", "--root", "/tmp/runsc-baseline",
             "do", "--", "sleep", "60"],
            stdin=subprocess.PIPE if pw else None,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if pw:
            assert p.stdin is not None
            p.stdin.write((pw + "\n").encode())
            p.stdin.flush()
        time.sleep(1.0)
        try:
            r = _sh(["pgrep", "-f", "runsc.*--network=none"])
            best = 0.0
            for pid in r.stdout.split():
                if not pid.isdigit():
                    continue
                ps = _sh(["ps", "-o", "rss=", "-p", pid])
                try:
                    best = max(best, float(ps.stdout.strip()) / 1024)
                except ValueError:
                    continue
            return best or None
        finally:
            p.terminate()
    if baseline == "terra":
        # RSS of the CH + virtiofsd pair serving the harness tenant.
        # smaps_rollup needs root (the daemon's processes are root-owned);
        # ps RSS is world-readable and consistent across baselines.
        from terra.client import TerraClient

        vms = TerraClient().vm_list().get("vms", [])
        if not vms:
            return None
        vm = vms[0].get("name", "")
        needles = [f"terra-{vm}-fs.sock", vm]
        total = 0.0
        for p in Path("/proc").iterdir():
            if not p.name.isdigit():
                continue
            try:
                cmdline = (p / "cmdline").read_bytes().decode(errors="ignore")
            except OSError:
                continue
            if not any(n in cmdline for n in needles):
                continue
            ps = _sh(["ps", "-o", "rss=", "-p", p.name])
            try:
                total += float(ps.stdout.strip()) / 1024
            except ValueError:
                continue
        return total if total else None
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="/tmp/baseline-compare.json")
    ap.add_argument("--baselines", default="bare,docker-default,docker-hardened,gvisor,terra")
    ap.add_argument("--no-perf", action="store_true")
    args = ap.parse_args()

    if not PROBE_BIN.exists():
        r = _sh(["bash", str(PROBES_DIR / "build-probes.sh")])
        if r.returncode != 0:
            print("cannot build escape probe:", r.stderr, file=sys.stderr)
            return 1

    avail = _available()
    selected = [b for b in args.baselines.split(",") if avail.get(b, False)]
    print("baselines:", selected)

    terra = TerraRunner() if "terra" in selected else None
    results: dict = {"host": os.uname().nodename, "workloads": {}}
    try:
        for name, argv in WORKLOADS:
            row = {}
            for b in selected:
                try:
                    r = run_workload(b, argv, terra)
                    row[b] = {
                        "rc": r["rc"],
                        "stdout": (r["stdout"] or "").strip()[-200:],
                        "stderr": (r["stderr"] or "").strip()[-200:],
                    }
                except Exception as e:  # noqa: BLE001
                    row[b] = {"error": str(e)[:200]}
            results["workloads"][name] = row
            print(f"  {name}: " + " | ".join(f"{b}={row[b].get('rc', 'ERR')}" for b in selected))

        if not args.no_perf:
            results["perf"] = {}
            for b in selected:
                results["perf"][b] = {
                    "cold_start_ms": _cold_start_ms(b, terra),
                    "exec_latency_ms": _exec_latency_ms(b, terra),
                    "per_instance_mem_mb": _memory_mb(b),
                }
            print("perf:", json.dumps(results["perf"], indent=2))
    finally:
        if terra is not None:
            terra.close()

    Path(args.out).write_text(json.dumps(results, indent=2))
    print("wrote:", args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

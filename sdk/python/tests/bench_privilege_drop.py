#!/usr/bin/env python3
"""A/B micro-benchmark: vmm privilege-drop mode vs legacy root mode.

Answers the review question the L1 change raises: does running CH and
virtiofsd under the dedicated ``terra-vmm`` user (fd-backed taps, chowned
export tree, uid translation) cost anything on the hot paths?

Same host, same guest assets, same code — only ``TERRA_VMM_USER`` differs
between the two modes (a missing user makes the daemon fall back to the
legacy root mode with a warning). Each mode gets a fresh daemon + state
dir and a warm-up VM (tap pool fill, layer chown, erofs mounts all
happen once during warm-up, not in the measurements).

Metrics:
  exec_latency_ms     — warm VM, blocking exec of ``echo hi``, p50/p95/mean
  exec_throughput     — 16 concurrent execs/s on one VM
  cold_create_ms      — create VM (layered fs + net), p50/p95
  restore_ms          — snapshot → restore (layered fs + net), p50/p95

Usage (root, KVM required):
    sudo python3 sdk/python/tests/bench_privilege_drop.py --out /tmp/pd.json
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "sdk" / "python"))

from terra.client import TerraClient  # noqa: E402
from terra.vm import create  # noqa: E402

KERNEL = REPO / "target/guest/vmlinux.bin"
INITRAMFS = REPO / "target/guest/alpine.cpio"
IRFS_VIRTIOFS = REPO / "target/guest/initramfs-virtiofs.cpio.gz"


def _sh(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def _ensure_vmm_user() -> None:
    r = _sh(["id", "terra-vmm"])
    if r.returncode == 0:
        return
    subprocess.run(
        [
            "useradd", "--system", "--no-create-home",
            "--shell", "/usr/sbin/nologin", "terra-vmm",
        ],
        check=True,
    )


def _pct(xs: list[float], p: float) -> float:
    s = sorted(xs)
    if not s:
        return 0.0
    k = max(0, min(len(s) - 1, int(round(p / 100 * (len(s) - 1)))))
    return s[k]


class Bench:
    def __init__(self, mode: str, state: Path, socket: str):
        self.mode = mode
        self.state = state
        self.socket = socket
        self.vmm_user = "terra-vmm" if mode == "vmm" else "terra-vmm-nonexistent"
        self.created: list[str] = []
        # VM names must be unique per mode: two embedded daemons share the
        # /tmp/terra-<name>-*.sock namespace (api/vsock/fs sockets).
        self.prefix = mode

    def _n(self, name: str) -> str:
        return f"{name}-{self.prefix}"

    def start(self, ch: str, vfsd: str, layer_dir: Path, snap_dir: Path) -> None:
        if Path(self.socket).exists():
            Path(self.socket).unlink()
        os.environ.update({
            "TERRA_STATE_DIR": str(self.state / "vms"),
            "TERRA_LAYER_DIR": str(layer_dir),
            "TERRA_SNAPSHOT_DIR": str(snap_dir),
            "TERRA_CH_BINARY": ch,
            "TERRA_VIRTIOFSD": vfsd,
            "TERRA_VMM_USER": self.vmm_user,
        })
        import terrarium_engine
        terrarium_engine.start_daemon(self.socket, ch_binary=ch)
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                self.client = TerraClient(socket_path=self.socket)
                self.client.vm_list()
                return
            except Exception:
                time.sleep(0.2)
        raise RuntimeError(f"{self.mode}: daemon did not come up")

    def _create(self, name: str) -> None:
        create(
            name, str(KERNEL),
            initramfs=str(IRFS_VIRTIOFS),
            cpus=1, memory_mb=256,
            layers=["marker", "base"],
            net=True,
            client=self.client,
        )
        self.created.append(name)

    def _exec(self, name: str) -> float:
        t0 = time.perf_counter()
        self.client.vm_exec(name, ["echo", "hi"])
        return (time.perf_counter() - t0) * 1000

    def warmup(self) -> str:
        self._create(self._n("wu"))
        # one exec to let the agent settle
        for _ in range(3):
            self._exec(self._n("wu"))
        return self._n("wu")

    def bench_exec_latency(self, vm: str, n: int = 40) -> dict:
        xs = [self._exec(vm) for _ in range(n)]
        return {
            "p50_ms": round(_pct(xs, 50), 2),
            "p95_ms": round(_pct(xs, 95), 2),
            "mean_ms": round(statistics.fmean(xs), 2),
        }

    def bench_exec_throughput(self, vm: str, workers: int = 16, rounds: int = 8) -> float:
        def one(_: int) -> None:
            self._exec(vm)

        t0 = time.perf_counter()
        with ThreadPoolExecutor(max_workers=workers) as ex:
            for _ in range(rounds):
                list(ex.map(one, range(workers)))
        dt = time.perf_counter() - t0
        return round((workers * rounds) / dt, 1)

    def bench_cold_create(self, n: int = 8) -> dict:
        xs: list[float] = []
        for i in range(n):
            name = self._n(f"c{i}")
            t0 = time.perf_counter()
            self._create(name)
            xs.append((time.perf_counter() - t0) * 1000)
            self.client.vm_destroy(name)
            self.created.remove(name)
        return {
            "p50_ms": round(_pct(xs, 50), 1),
            "p95_ms": round(_pct(xs, 95), 1),
            "mean_ms": round(statistics.fmean(xs), 1),
        }

    def bench_restore(self, vm: str, n: int = 6) -> dict:
        snap = self.state / "snapshots" / "keep"
        self.client.vm_snapshot(vm, snapshot_path=str(snap))
        xs: list[float] = []
        for i in range(n):
            name = self._n(f"r{i}")
            t0 = time.perf_counter()
            self.client.vm_restore(
                name, str(snap),
                layers=["marker", "base"],
                net=True,
            )
            self.created.append(name)
            xs.append((time.perf_counter() - t0) * 1000)
            self.client.vm_destroy(name)
            self.created.remove(name)
        return {
            "p50_ms": round(_pct(xs, 50), 1),
            "p95_ms": round(_pct(xs, 95), 1),
            "mean_ms": round(statistics.fmean(xs), 1),
        }

    def cleanup(self) -> None:
        for name in list(self.created):
            try:
                self.client.vm_destroy(name)
            except Exception:
                pass


def _prepare_assets(state: Path, initramfs: Path) -> Path:
    layer = state / "layers"
    (layer / "base").mkdir(parents=True)
    subprocess.run(
        f"zcat {initramfs} | cpio -idm --quiet",
        shell=True, cwd=layer / "base", check=True,
    )
    marker = layer / "marker" / "usr" / "bin"
    marker.mkdir(parents=True)
    (marker / "hello.py").write_text("print('hello')\n")
    (state / "snapshots").mkdir()
    return layer


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--modes", default="vmm,legacy")
    ap.add_argument("--out", default=None)
    ap.add_argument("--exec-samples", type=int, default=40)
    ap.add_argument("--creates", type=int, default=8)
    ap.add_argument("--restores", type=int, default=6)
    args = ap.parse_args()

    if os.geteuid() != 0:
        print("must run as root", file=sys.stderr)
        return 2
    for p, hint in (
        (KERNEL.exists(), f"{KERNEL} missing"),
        (INITRAMFS.exists(), f"{INITRAMFS} missing"),
        (IRFS_VIRTIOFS.exists(), f"{IRFS_VIRTIOFS} missing"),
    ):
        if not p:
            print(f"preflight: {hint}", file=sys.stderr)
            return 2
    _ensure_vmm_user()
    ch = shutil.which("cloud-hypervisor") or "/home/liujinyao/.local/bin/cloud-hypervisor"
    vfsd = (
        os.environ.get("TERRA_VIRTIOFSD")
        or shutil.which("virtiofsd")
        or str(Path.home() / ".cargo/bin/virtiofsd")
    )
    if not Path(ch).exists() or not Path(vfsd).exists():
        print(f"preflight: ch={ch} vfsd={vfsd} missing", file=sys.stderr)
        return 2

    results: dict[str, dict] = {}
    for mode in [m.strip() for m in args.modes.split(",") if m.strip()]:
        state = Path(tempfile.mkdtemp(prefix=f"terra-bench-{mode}-"))
        state.chmod(0o755)
        layer = _prepare_assets(state, INITRAMFS)
        socket = f"/tmp/terra-bench-{mode}.sock"
        b = Bench(mode, state, socket)
        b.start(ch, vfsd, layer, state / "snapshots")
        try:
            wu = b.warmup()
            exec_lat = b.bench_exec_latency(wu, args.exec_samples)
            thr = b.bench_exec_throughput(wu)
            create_ms = b.bench_cold_create(args.creates)
            restore_ms = b.bench_restore(wu, args.restores)
            results[mode] = {
                "exec_latency": exec_lat,
                "exec_throughput": thr,
                "cold_create": create_ms,
                "restore": restore_ms,
            }
            print(f"[{mode}] {json.dumps(results[mode])}")
        finally:
            b.cleanup()
            if not results.get("keep_state"):
                shutil.rmtree(state, ignore_errors=True)
            try:
                Path(socket).unlink()
            except FileNotFoundError:
                pass

    # summary table
    print("\n=== A/B summary (vmm vs legacy root mode) ===")
    for metric in ("exec_latency", "cold_create", "restore"):
        print(
            f"{metric:14s} "
            + "  ".join(
                f"{mode}: p50={results[mode][metric]['p50_ms']}ms "
                f"p95={results[mode][metric]['p95_ms']}ms"
                for mode in results
            )
        )
    print("exec_throughput: " + "  ".join(
        f"{mode}: {results[mode]['exec_throughput']} execs/s" for mode in results
    ))

    if args.out:
        out = {
            "method": "A/B same-host same-code; TERRA_VMM_USER flips vmm/legacy",
            "host": os.uname().nodename,
            "results": results,
        }
        Path(args.out).write_text(json.dumps(out, indent=2))
        print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

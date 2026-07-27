#!/usr/bin/env python3
"""Terrarium end-to-end application demo — warm pool + layered rootfs.

Real KVM, real Cloud Hypervisor, no mocks. Demonstrates the product story:

  1. build filesystem layers (base + tools)
  2. cold-boot a VM from composed layers            (timed)
  3. warm-pool: claim an idle VM + hot-plug layers  (timed — the point)
  4. execute commands INSIDE the guest via the SDK
  5. release and re-claim: pool reuse

Usage:
  python3 sdk/python/examples/warm_pool_demo.py

Env overrides: TERRA_CH_BINARY, TERRA_VIRTIOFSD
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "sdk" / "python"))

from terra.client import TerraClient  # noqa: E402
from terra.vm import create  # noqa: E402

KERNEL = REPO / "target/guest/vmlinux.bin"
ALPINE = REPO / "target/guest/alpine.cpio"
IRFS_VIRTIOFS = REPO / "target/guest/initramfs-virtiofs.cpio.gz"
IRFS_AGENT = REPO / "target/guest/initramfs-agent.cpio.gz"
SOCKET = "/tmp/terra-demo.sock"


def step(msg: str) -> None:
    print(f"\n\033[1m== {msg}\033[0m")


def find_binary(candidates: list[str]) -> str | None:
    for c in candidates:
        if c and Path(c).exists():
            return c
    return None


def exec_retry(client: TerraClient, name: str, args: list[str], tries: int = 20) -> dict:
    """Exec with retry — the guest agent takes a moment to come up after
    create returns (CH reports Running before guest userspace is ready)."""
    last: Exception | None = None
    for _ in range(tries):
        try:
            return client.vm_exec(name, args)
        except Exception as e:  # noqa: BLE001
            last = e
            time.sleep(0.5)
    raise last  # type: ignore[misc]


def main() -> int:
    step("Preflight")
    ch = find_binary(
        [os.environ.get("TERRA_CH_BINARY", ""), "/tmp/cloud-hypervisor-static", shutil.which("cloud-hypervisor") or ""]
    )
    vfsd = find_binary(
        [
            os.environ.get("TERRA_VIRTIOFSD", ""),
            shutil.which("virtiofsd") or "",
            str(Path.home() / ".cargo/bin/virtiofsd"),
            "/usr/lib/qemu/virtiofsd",
        ]
    )
    for ok, name in [(Path("/dev/kvm").exists(), "/dev/kvm"), (ch, "cloud-hypervisor"), (vfsd, "virtiofsd")]:
        if not ok:
            print(f"MISSING: {name}")
            return 1
    print(f"CH: {ch}\nvirtiofsd: {vfsd}")
    for img, builder in [
        (IRFS_VIRTIOFS, "images/build-initramfs-virtiofs.sh"),
        (IRFS_AGENT, "images/build-initramfs-agent.sh"),
    ]:
        if not img.exists():
            subprocess.run(["bash", builder], cwd=REPO, check=True)
    if not KERNEL.exists() or not ALPINE.exists():
        print("MISSING guest images — run images/build.sh first")
        return 1

    work = Path(tempfile.mkdtemp(prefix="terra-demo-"))
    state_dir = work / "state"
    layer_dir = work / "layers"

    step("Build filesystem layers (base + tools)")
    base = layer_dir / "base"
    base.mkdir(parents=True)
    subprocess.run(f"zcat {ALPINE} | cpio -idm --quiet", shell=True, cwd=base, check=True)
    # inject the guest agent so exec works in cold-booted layered VMs too
    gp = REPO / "target/x86_64-unknown-linux-musl/release/guest-proxy"
    if not gp.exists():
        subprocess.run(
            ["cargo", "build", "--release", "--target",
             "x86_64-unknown-linux-musl", "-p", "guest-proxy"],
            cwd=REPO, check=True,
        )
    shutil.copy(gp, base / "bin" / "guest-proxy")
    init_file = base / "init"
    init_text = init_file.read_text()
    if "guest-proxy" not in init_text:
        init_text = init_text.replace(
            "exec /bin/sh", "/bin/guest-proxy &\nexec /bin/sh"
        )
        init_file.write_text(init_text)
    tools = layer_dir / "tools" / "usr" / "bin"
    tools.mkdir(parents=True)
    (tools / "terra-info.sh").write_text(
        '#!/bin/sh\necho "hello from the tools layer"\n'
    )
    subprocess.run(["chmod", "+x", str(tools / "terra-info.sh")], check=True)
    print(f"layers: {sorted(p.name for p in layer_dir.iterdir())}")

    step("Start engine daemon")
    if Path(SOCKET).exists():
        Path(SOCKET).unlink()
    env = {
        **os.environ,
        "TERRA_STATE_DIR": str(state_dir),
        "TERRA_CH_BINARY": ch,
        "TERRA_VIRTIOFSD": vfsd,
        "TERRA_LAYER_DIR": str(layer_dir),
    }
    daemon = subprocess.Popen(
        [str(REPO / "target/release/engine"), "daemon", "--socket", SOCKET],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    for _ in range(50):
        if Path(SOCKET).exists():
            break
        time.sleep(0.2)
    client = TerraClient(socket_path=SOCKET)
    print(f"daemon pid={daemon.pid}")

    vms_created: list[str] = []
    try:
        step("Cold boot: VM from composed layers")
        t0 = time.monotonic()
        vm = create(
            "demo-cold", str(KERNEL),
            initramfs=str(IRFS_VIRTIOFS),
            cpus=1, memory_mb=256,
            layers=["tools", "base"],
            client=client,
        )
        vms_created.append("demo-cold")
        cold_ms = (time.monotonic() - t0) * 1000
        print(f"created in {cold_ms:.0f} ms — state={vm.info()['state']}")
        r = exec_retry(client, "demo-cold", ["cat", "/usr/bin/terra-info.sh"])
        print("guest says:", r["stdout"].strip())
        client.vm_destroy("demo-cold")
        vms_created.remove("demo-cold")

        step("Warm pool: 2 idle VMs standing by")
        print(client.pool_create(2, kernel=str(KERNEL)))
        vms_created.extend(client.pool_list()["pool"][i]["name"] for i in range(2))

        step("Claim #1: hot-plug layers into an idle VM")
        t0 = time.monotonic()
        claim = client.pool_claim(["tools", "base"])
        claim_ms = (time.monotonic() - t0) * 1000
        name = claim["name"]
        print(f"claimed {name} in {claim_ms:.0f} ms")

        step("Execute commands inside the guest (SDK -> engine -> vsock)")
        for args in (["ls", "/newroot"], ["ls", "/newroot/usr/bin"], ["cat", "/newroot/etc/alpine-release"]):
            r = exec_retry(client, name, args, tries=3)
            print(f"$ {' '.join(args)}\n  {r['stdout'].strip()}")

        step("Release, then claim #2 (pool reuse)")
        client.pool_release(name)
        claim2 = client.pool_claim(["tools", "base"])
        print(f"re-claimed {claim2['name']}")
        print("pool:", client.pool_list()["pool"])

        step("Timing summary")
        print(f"cold boot with layers : {cold_ms:.0f} ms")
        print(f"warm claim (hot-plug) : {claim_ms:.0f} ms")
        print(
            "\nnote: cold 'created' returns when the CH API socket appears\n"
            "(guest kernel not yet running), while a warm claim completes\n"
            "compose + hot-plug + guest mount — real work done. The pool's\n"
            "win is a pre-booted, agent-ready VM, not raw API latency."
        )
    finally:
        step("Teardown")
        for n in list(vms_created):
            try:
                client.vm_destroy(n)
            except Exception:
                pass
        daemon.send_signal(signal.SIGTERM)
        daemon.wait(timeout=15)
        shutil.rmtree(work, ignore_errors=True)
        print("daemon stopped, no leaks")
    return 0


if __name__ == "__main__":
    sys.exit(main())

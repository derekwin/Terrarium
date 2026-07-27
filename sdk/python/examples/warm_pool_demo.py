#!/usr/bin/env python3
"""Terrarium end-to-end application demo — warm pool + layered rootfs.

Zero-setup version: the SDK manages everything (daemon, binaries, dirs).

    python3 sdk/python/examples/warm_pool_demo.py
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "sdk" / "python"))

from terra import images  # noqa: E402
from terra.client import TerraClient  # noqa: E402
from terra.daemon import Daemon  # noqa: E402
from terra.vm import create  # noqa: E402


def step(msg: str) -> None:
    print(f"\n\033[1m== {msg}\033[0m")


def exec_retry(client: TerraClient, name: str, args: list[str], tries: int = 20) -> dict:
    last: Exception | None = None
    for _ in range(tries):
        try:
            return client.vm_exec(name, args)
        except Exception as e:  # noqa: BLE001
            last = e
            time.sleep(0.5)
    raise last  # type: ignore[misc]


def main() -> int:
    step("Preflight (SDK resolves binaries & images automatically)")
    kernel = images.ensure("vmlinux.bin")
    alpine = images.ensure("alpine.cpio")
    irfs_virtiofs = images.ensure("initramfs-virtiofs.cpio.gz")
    irfs_agent = images.ensure("initramfs-agent.cpio.gz")
    print(f"kernel: {kernel}")

    work = Path(tempfile.mkdtemp(prefix="terra-demo-"))

    step("Build filesystem layers (base + tools)")
    base = work / "layers" / "base"
    base.mkdir(parents=True)
    subprocess.run(f"zcat {alpine} | cpio -idm --quiet", shell=True, cwd=base, check=True)
    # inject the guest agent so exec works in cold-booted layered VMs too
    gp = REPO / "target/x86_64-unknown-linux-musl/release/guest-proxy"
    if gp.exists():
        shutil.copy(gp, base / "bin" / "guest-proxy")
        init_file = base / "init"
        init_text = init_file.read_text()
        if "guest-proxy" not in init_text:
            init_file.write_text(
                init_text.replace("exec /bin/sh", "/bin/guest-proxy &\nexec /bin/sh")
            )
    tools = work / "layers" / "tools" / "usr" / "bin"
    tools.mkdir(parents=True)
    (tools / "terra-info.sh").write_text('#!/bin/sh\necho "hello from the tools layer"\n')
    subprocess.run(["chmod", "+x", str(tools / "terra-info.sh")], check=True)
    print("layers: base, tools")

    vms_created: list[str] = []
    with Daemon(kernel=str(kernel), layer_dir=str(work / "layers")) as d:
        client = TerraClient(socket_path=d.socket)
        print(f"daemon pid={d.pid}, socket={d.socket}")
        try:
            step("Cold boot: VM from composed layers")
            t0 = time.monotonic()
            vm = create(
                "demo-cold", str(kernel),
                initramfs=str(irfs_virtiofs),
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
            print(client.pool_create(2, kernel=str(kernel)))
            vms_created.extend(s["name"] for s in client.pool_list()["pool"])

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
            for n in list(vms_created):
                try:
                    client.vm_destroy(n)
                except Exception:
                    pass
    print("\ndaemon stopped cleanly")
    shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""terra — Terrarium CLI (Python).

    python -m terra <command> [args]
    # or, once pip-installed: terra <command> [args]

Admin/user command line — daemon operations go through the engine
socket (TERRA_SOCKET or managed default); host-side image operations
run locally.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

from . import images, paths
from .client import TerraClient, TerraError


def _client(args) -> TerraClient:
    addr = args.socket
    if addr is None:
        return TerraClient()
    if addr.startswith("tcp://"):
        return TerraClient(addr, token=os.environ.get("TERRA_TOKEN"))
    return TerraClient(addr)


def _print(resp) -> int:
    print(json.dumps(resp, indent=2, ensure_ascii=False))
    return 0


def _err(msg: str) -> int:
    print(f"ERROR: {msg}", file=sys.stderr)
    return 1


# ---------------------------------------------------------------------------
# daemon commands
# ---------------------------------------------------------------------------
def cmd_list(args):
    return _print(_client(args).vm_list())


def cmd_info(args):
    return _print(_client(args).vm_info(args.name))


def cmd_create(args):
    c = _client(args)
    resp = c.vm_create(
        args.name,
        args.kernel,
        initramfs=args.initramfs,
        cpus=args.cpus,
        max_cpus=args.max_cpus,
        memory_mb=args.memory,
        max_memory_mb=args.max_memory,
        layers=args.layers or None,
        upper=args.upper,
        net=args.net,
    )
    return _print(resp)


def cmd_exec(args):
    return _print(_client(args).vm_exec(args.name, args.args, timeout_secs=args.timeout))


def cmd_resize(args):
    c = _client(args)
    if args.cpus is None and args.memory_bytes is None:
        return _err("resize needs --cpus and/or --memory-bytes")
    return _print(c.vm_resize(args.name, cpus=args.cpus, memory_bytes=args.memory_bytes))


def _simple(method: str):
    def f(args):
        return _print(getattr(_client(args), method)(args.name))

    return f


def cmd_pool_create(args):
    return _print(_client(args).pool_create(args.size, kernel=args.kernel, net=args.net))


def cmd_pool_list(args):
    return _print(_client(args).pool_list())


def cmd_pool_claim(args):
    return _print(_client(args).pool_claim(args.layers))


def _simple_pool_release(args):
    return _print(_client(args).pool_release(args.name))


def cmd_attach_fs(args):
    return _print(_client(args).vm_attach_fs(args.name, args.layers))


def _simple_detach(args):
    return _print(_client(args).vm_detach_fs(args.name))


def cmd_net_list(args):
    return _print(_client(args)._send({"command": "net_list"}))


def cmd_net_down(args):
    return _print(_client(args)._send({"command": "net_down"}))


# ---------------------------------------------------------------------------
# image commands (host-side)
# ---------------------------------------------------------------------------
def cmd_image_layers(args):
    layer_dir = os.environ.get("TERRA_LAYER_DIR") or str(paths.layers_dir())
    try:
        for e in sorted(Path(layer_dir).iterdir()):
            print(e.name)
        return 0
    except OSError as e:
        return _err(f"read {layer_dir}: {e}")


def cmd_image_layer(args):
    out = images.build_layer(args.src, args.name)
    print(f"layer built: {out}")
    return 0


def _run_builder(script: str) -> str | None:
    if Path(script).exists():
        return script
    return None


_BUILDER_SCRIPTS = {
    "kernel": "images/build-kernel.sh",
    "rootfs": "images/build-rootfs.sh",
    "initramfs": "images/build-initramfs-virtiofs.sh",
    "agent-initramfs": "images/build-initramfs-agent.sh",
}


def cmd_image_build(args):
    script = _BUILDER_SCRIPTS[args.what]
    if not Path(script).exists():
        return _err(f"{script} not found — run from the Terrarium repo root")
    import subprocess

    extra = [args.version] if getattr(args, "version", None) else []
    r = subprocess.run(["bash", script, *extra])
    return r.returncode


def cmd_image_layer_build(args):
    """Build a tool layer by configuring inside a builder VM."""
    client = _client(args)
    name = args.name
    builder = f"lb-{name}"

    # 1) builder VM from the base layer, persistent upper
    try:
        client.vm_create(
            builder,
            args.kernel,
            initramfs=args.initramfs,
            cpus=1,
            memory_mb=512,
            layers=[args.base],
            upper=builder,
            net=not args.no_net,
        )
    except TerraError as e:
        return _err(f"builder VM create failed: {e}")
    print(f"builder VM {builder} running")

    try:
        content = Path(args.script).read_text()
    except OSError as e:
        return _err(f"read script: {e}")

    # 2) run setup inside (vm_exec retries the agent boot window)
    resp = client.vm_exec(builder, ["sh", "-c", content], timeout_secs=args.timeout)
    if resp.get("exit_code") != 0:
        try:
            client.vm_destroy(builder)
        except TerraError:
            pass
        return _err(f"setup script failed: {resp}")
    print("setup output:", resp)

    # 3) cleanup runtime noise
    client.vm_exec(
        builder,
        ["sh", "-c", "rm -rf /tmp/* /run/* /var/log/* /etc/resolv.conf 2>/dev/null; sync"],
        timeout_secs=30,
    )

    # 4) destroy builder
    try:
        client.vm_destroy(builder)
    except TerraError:
        pass
    print("builder VM destroyed")

    # 5) pack the upperdir delta as the layer
    fs_root = os.environ.get("TERRA_STATE_DIR", "/tmp/terra-disks")
    upper_dir = Path(fs_root) / "fs" / "uppers" / builder
    if not upper_dir.is_dir():
        return _err(
            f"upperdir {upper_dir} not found — layer-build needs a LOCAL daemon "
            "(the upperdir lives on the daemon host)"
        )
    out = images.build_layer(str(upper_dir), name)
    print(f"layer '{name}' built: {out}")
    return 0


# ---------------------------------------------------------------------------
def main() -> int:
    p = argparse.ArgumentParser(prog="terra", description="Terrarium CLI (python -m terra)")
    p.add_argument("--socket", help="daemon socket path or tcp://host:port")
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("list").set_defaults(f=cmd_list)
    sp = sub.add_parser("info")
    sp.add_argument("name")
    sp.set_defaults(f=cmd_info)

    sp = sub.add_parser("create")
    sp.add_argument("name")
    sp.add_argument("--kernel", required=True)
    sp.add_argument("--initramfs")
    sp.add_argument("--cpus", type=int, default=2)
    sp.add_argument("--max-cpus", type=int)
    sp.add_argument("--memory", type=int, default=512)
    sp.add_argument("--max-memory", type=int)
    sp.add_argument("--layers", nargs="*", default=[])
    sp.add_argument("--upper")
    sp.add_argument("--net", action="store_true")
    sp.set_defaults(f=cmd_create)

    sp = sub.add_parser("exec")
    sp.add_argument("name")
    sp.add_argument("--timeout", type=int, default=60)
    sp.add_argument("args", nargs=argparse.REMAINDER)
    sp.set_defaults(f=cmd_exec)

    sp = sub.add_parser("resize")
    sp.add_argument("name")
    sp.add_argument("--cpus", type=int)
    sp.add_argument("--memory-bytes", type=int)
    sp.set_defaults(f=cmd_resize)

    for name, method in (
        ("shutdown", "vm_shutdown"),
        ("kill", "vm_kill"),
        ("destroy", "vm_destroy"),
    ):
        sp = sub.add_parser(name)
        sp.add_argument("name")
        sp.set_defaults(f=_simple(method))

    sp = sub.add_parser("pool-create")
    sp.add_argument("--size", type=int, default=1)
    sp.add_argument("--kernel")
    sp.add_argument("--net", action="store_true")
    sp.set_defaults(f=cmd_pool_create)

    sub.add_parser("pool-list").set_defaults(f=cmd_pool_list)

    sp = sub.add_parser("pool-claim")
    sp.add_argument("--layers", nargs="+", required=True)
    sp.set_defaults(f=cmd_pool_claim)

    sp = sub.add_parser("pool-release")
    sp.add_argument("name")
    sp.set_defaults(f=_simple_pool_release)

    sp = sub.add_parser("attach-fs")
    sp.add_argument("name")
    sp.add_argument("--layers", nargs="+", required=True)
    sp.set_defaults(f=cmd_attach_fs)

    sp = sub.add_parser("detach-fs")
    sp.add_argument("name")
    sp.set_defaults(f=_simple_detach)

    sub.add_parser("net-list").set_defaults(f=cmd_net_list)
    sub.add_parser("net-down").set_defaults(f=cmd_net_down)

    img = sub.add_parser("image")
    isub = img.add_subparsers(dest="image_cmd", required=True)

    isub.add_parser("layers").set_defaults(f=cmd_image_layers)

    sp = isub.add_parser("layer")
    sp.add_argument("src")
    sp.add_argument("name")
    sp.set_defaults(f=cmd_image_layer)

    sp = isub.add_parser("layer-build")
    sp.add_argument("name")
    sp.add_argument("--script", required=True)
    sp.add_argument("--base", default="base")
    sp.add_argument("--kernel", default="target/guest/vmlinux.bin")
    sp.add_argument("--initramfs", default="target/guest/initramfs-virtiofs.cpio.gz")
    sp.add_argument("--no-net", action="store_true")
    sp.add_argument("--timeout", type=int, default=600)
    sp.set_defaults(f=cmd_image_layer_build)

    for what in ("kernel", "rootfs", "initramfs", "agent-initramfs"):
        sp = isub.add_parser(what)
        if what == "kernel":
            sp.add_argument("--version")
        sp.set_defaults(f=cmd_image_build, what=what)

    args = p.parse_args()
    try:
        return args.f(args)
    except TerraError as e:
        return _err(str(e))
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())

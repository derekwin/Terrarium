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
    kernel = args.kernel
    if kernel and not Path(kernel).exists():
        kernel = str(images.resolve_kernel(kernel))
    rootfs = args.rootfs
    if rootfs and not Path(rootfs).exists():
        rootfs = str(images.resolve_rootfs(rootfs))
    resp = c.vm_create(
        args.name,
        kernel,
        initramfs=rootfs,
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
def cmd_daemon_start(args):
    """Start a daemon as a detached background process (same Python, no sudo)."""
    import subprocess

    cmd = [
        sys.executable,
        "-c",
        "from terra.daemon import Daemon; import time\n"
        "d = Daemon(tcp=%r).start()\n"
        "print(d.socket, flush=True)\n"
        "time.sleep(10**9)" % (args.tcp,),
    ]
    proc = subprocess.Popen(
        cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True
    )
    from . import paths

    (paths.run_dir() / "daemon.pid").write_text(str(proc.pid))
    time.sleep(1.5)
    sock = paths.default_socket()
    print(f"daemon started (pid={proc.pid}, socket={sock})")
    return 0


def cmd_image_base(args):
    """Build/refresh the base layer in the managed layers dir.

    Extracts the managed alpine rootfs into layers/<name> (default:
    "base") so `layers=["base"]` resolves with zero env setup.
    """
    import shutil
    import subprocess

    name = args.name
    dest = paths.layers_dir() / name
    if dest.exists() and not args.force:
        print(f"{dest} exists (use --force to rebuild)")
        return 0
    shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True)
    cpio = images.ensure("alpine.cpio")
    r = subprocess.run(
        f"zcat {cpio} | cpio -idm --quiet",
        shell=True,
        cwd=dest,
    )
    if r.returncode:
        return r.returncode
    print(f"base layer ready: {dest}")
    return 0


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
    import tempfile

    name = getattr(args, "name", None)
    if name:
        # Named variant: build into a temp dir, then move the artifact
        # into the managed images dir under its name.
        with tempfile.TemporaryDirectory() as td:
            extra = []
            if args.what == "kernel":
                # build-kernel.sh [version] [config] [output_dir];
                # empty strings fall back to the script's defaults.
                r = subprocess.run(
                    ["bash", script, args.version or "", "", td]
                )
                if r.returncode:
                    return r.returncode
                src = Path(td) / "vmlinux.bin"
            else:  # rootfs
                out_dir = Path(td) / "rootfs"
                r = subprocess.run(["bash", script, args.type, str(out_dir)])
                if r.returncode:
                    return r.returncode
                src = out_dir
            import shutil

            dest = (
                paths.kernels_dir() / name
                if args.what == "kernel"
                else paths.rootfs_dir() / name
            )
            if dest.exists():
                shutil.rmtree(dest)
            dest.mkdir(parents=True)
            target = dest / src.name if src.is_file() else dest / "rootfs"
            shutil.move(str(src), str(target))
            print(f"built: {target}")
            return 0

    extra = [args.version] if getattr(args, "version", None) else []
    if args.what == "rootfs" and getattr(args, "type", None):
        extra = [args.type]
    r = subprocess.run(["bash", script, *extra])
    return r.returncode


def cmd_image_layer_build(args):
    """Build a tool layer by configuring inside a builder VM."""
    # Preflight: networked builds need a privileged daemon — offer the
    # two ways out before burning a builder VM.
    if not args.no_net and os.geteuid() != 0:
        return _err(
            "this build uses a networked builder VM (downloads), which needs "
            "CAP_NET_ADMIN.\n"
            "  either: sudo terra daemon start   (then retry)\n"
            "  or:     add --no-net for an offline build"
        )
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
# resource-group handlers (kernel/rootfs/layer/net/daemon)
# ---------------------------------------------------------------------------
def _variant_ls_dir(path: Path, kinds=(".erofs",)) -> int:
    if not path.is_dir():
        print(f"(empty: {path})")
        return 0
    for e in sorted(path.iterdir()):
        tag = "/" if e.is_dir() else ""
        print(f"{e.name}{tag}")
    return 0


def _kernel_variants() -> list[str]:
    """Kernel artifacts: vmlinux.bin and named dirs containing a kernel."""
    out = []
    for e in sorted(paths.images_dir().iterdir()):
        if e.name == "vmlinux.bin":
            out.append("vmlinux.bin (default)")
        elif e.is_dir() and (e / "vmlinux.bin").exists():
            out.append(f"{e.name}/")
    return out


def _rootfs_variants() -> list[str]:
    """Rootfs/initramfs artifacts in images/rootfs/."""
    out = []
    for e in sorted(paths.rootfs_dir().iterdir()):
        out.append(f"{e.name}/" if e.is_dir() else e.name)
    return out


def cmd_kernel_ls(args):
    for e in sorted(paths.kernels_dir().iterdir()):
        if e.is_dir() and (e / "vmlinux.bin").exists():
            print(f"{e.name}/")
    return 0


def cmd_rootfs_ls(args):
    for line in _rootfs_variants():
        print(line)
    return 0


def _remove_path(path: Path) -> int:
    import shutil

    if not path.exists():
        return _err(f"not found: {path}")
    if path.is_dir():
        shutil.rmtree(path)
    else:
        path.unlink()
    print(f"removed {path}")
    return 0


def cmd_kernel_remove(args):
    return _remove_path(paths.kernels_dir() / args.name)


def cmd_rootfs_remove(args):
    return _remove_path(paths.rootfs_dir() / args.name)


def cmd_layer_remove(args):
    layer_dir = Path(os.environ.get("TERRA_LAYER_DIR") or paths.layers_dir())
    for cand in (layer_dir / args.name, layer_dir / f"{args.name}.erofs"):
        if cand.exists():
            return _remove_path(cand)
    return _err(f"layer '{args.name}' not found under {layer_dir}")


def cmd_layer_create(args):
    if args.from_dir:
        out = images.build_layer(args.from_dir, args.name)
        print(f"layer built: {out}")
        return 0
    if args.from_image:
        args2 = argparse.Namespace(name=args.name, force=True)
        return cmd_image_base(args2)
    return cmd_image_layer_build(args)  # --script path (build-by-doing)


def cmd_net_create(args):
    return _print(_client(args)._send({"command": "net_up"}))


def _daemon_pidfile() -> Path:
    return paths.run_dir() / "daemon.pid"


def cmd_daemon_ls(args):
    import json as _json

    info = {"socket": paths.default_socket()}
    pf = _daemon_pidfile()
    if pf.exists():
        info["pid"] = int(pf.read_text().strip())
        info["alive"] = Path(f"/proc/{info['pid']}").exists()
    else:
        info["alive"] = Path(info["socket"]).exists()
    print(_json.dumps(info, indent=2))
    return 0


def _daemon_stop(sig) -> int:
    import signal as _signal

    pf = _daemon_pidfile()
    if not pf.exists():
        return _err("no daemon.pid — was it started via 'terra daemon start'?")
    pid = int(pf.read_text().strip())
    try:
        os.kill(pid, sig)
        print(f"sent {_signal.Signals(sig).name} to daemon (pid={pid})")
        return 0
    except ProcessLookupError:
        pf.unlink(missing_ok=True)
        return _err(f"daemon pid {pid} not running (stale pidfile removed)")


def cmd_daemon_stop(args):
    import signal as _signal

    return _daemon_stop(_signal.SIGTERM)


def cmd_daemon_destroy(args):
    import signal as _signal

    rc = _daemon_stop(_signal.SIGKILL)
    _daemon_pidfile().unlink(missing_ok=True)
    try:
        Path(paths.default_socket()).unlink()
    except FileNotFoundError:
        pass
    return rc


# ---------------------------------------------------------------------------
def main() -> int:
    p = argparse.ArgumentParser(prog="terra", description="Terrarium CLI (python -m terra)")
    p.add_argument("--socket", help="daemon socket path or tcp://host:port")
    sub = p.add_subparsers(dest="cmd", required=True)

    # --- unified resource groups: vm/kernel/rootfs/layer/pool/net/daemon ---
    vm = sub.add_parser("vm", help="VM operations")
    vms = vm.add_subparsers(dest="action", required=True)
    vms.add_parser("ls").set_defaults(f=cmd_list)
    sp = vms.add_parser("create")
    sp.add_argument("name")
    sp.add_argument("--kernel", required=True)
    sp.add_argument("--rootfs", "--initramfs", dest="rootfs")
    sp.add_argument("--cpus", type=int, default=2)
    sp.add_argument("--max-cpus", type=int)
    sp.add_argument("--memory", type=int, default=512)
    sp.add_argument("--max-memory", type=int)
    sp.add_argument("--layers", nargs="*", default=[])
    sp.add_argument("--upper")
    sp.add_argument("--net", action="store_true")
    sp.set_defaults(f=cmd_create)
    for act, method in (
        ("remove", "vm_destroy"),
        ("info", None),
        ("exec", None),
        ("resize", None),
        ("shutdown", "vm_shutdown"),
        ("kill", "vm_kill"),
    ):
        sp = vms.add_parser(act)
        sp.add_argument("name")
        if act == "exec":
            sp.add_argument("--timeout", type=int, default=60)
            sp.add_argument("args", nargs=argparse.REMAINDER)
            sp.set_defaults(f=cmd_exec)
        elif act == "resize":
            sp.add_argument("--cpus", type=int)
            sp.add_argument("--memory-bytes", type=int)
            sp.set_defaults(f=cmd_resize)
        elif act == "info":
            sp.set_defaults(f=cmd_info)
        else:
            sp.set_defaults(f=_simple(method))

    sp = vms.add_parser("attach-fs")
    sp.add_argument("name")
    sp.add_argument("--layers", nargs="+", required=True)
    sp.set_defaults(f=cmd_attach_fs)
    sp = vms.add_parser("detach-fs")
    sp.add_argument("name")
    sp.set_defaults(f=_simple_detach)

    for kind in ("kernel", "rootfs"):
        g = sub.add_parser(kind, help=f"manage {kind} variants")
        gs = g.add_subparsers(dest="action", required=True)
        gs.add_parser("ls").set_defaults(f=cmd_kernel_ls if kind == "kernel" else cmd_rootfs_ls)
        c = gs.add_parser("create")
        c.add_argument("-n", "--name", default="default")
        if kind == "kernel":
            c.add_argument("--version")
        else:
            c.add_argument("--type", default="busybox")
        c.set_defaults(f=cmd_image_build, what=kind)
        r = gs.add_parser("remove")
        r.add_argument("-n", "--name", required=True)
        r.set_defaults(f=cmd_kernel_remove if kind == "kernel" else cmd_rootfs_remove)

    g = sub.add_parser("layer", help="manage filesystem layers")
    gs = g.add_subparsers(dest="action", required=True)
    gs.add_parser("ls").set_defaults(f=cmd_image_layers)
    c = gs.add_parser("create")
    c.add_argument("-n", "--name", required=True)
    src = c.add_mutually_exclusive_group(required=True)
    src.add_argument("--from-dir", help="pack an existing directory")
    src.add_argument("--script", help="build-by-doing: run setup in a builder VM")
    src.add_argument("--from-image", action="store_true", help="base layer from guest rootfs")
    c.add_argument("--base", default="base")
    c.add_argument("--kernel", default="target/guest/vmlinux.bin")
    c.add_argument("--initramfs", default="target/guest/initramfs-virtiofs.cpio.gz")
    c.add_argument("--no-net", action="store_true")
    c.add_argument("--timeout", type=int, default=600)
    c.set_defaults(f=cmd_layer_create)
    r = gs.add_parser("remove")
    r.add_argument("-n", "--name", required=True)
    r.set_defaults(f=cmd_layer_remove)

    g = sub.add_parser("pool", help="warm pool operations")
    gs = g.add_subparsers(dest="action", required=True)
    gs.add_parser("ls").set_defaults(f=cmd_pool_list)
    c = gs.add_parser("create")
    c.add_argument("--size", type=int, default=1)
    c.add_argument("--kernel")
    c.add_argument("--net", action="store_true")
    c.set_defaults(f=cmd_pool_create)
    r = gs.add_parser("remove")
    r.add_argument("-n", "--name", required=True)
    r.set_defaults(f=_simple("vm_destroy"))
    c = gs.add_parser("claim")
    c.add_argument("--layers", nargs="+", required=True)
    c.set_defaults(f=cmd_pool_claim)
    r = gs.add_parser("release")
    r.add_argument("name")
    r.set_defaults(f=_simple_pool_release)

    g = sub.add_parser("net", help="NAT networking")
    gs = g.add_subparsers(dest="action", required=True)
    gs.add_parser("ls").set_defaults(f=cmd_net_list)
    gs.add_parser("create").set_defaults(f=cmd_net_create)
    gs.add_parser("remove").set_defaults(f=cmd_net_down)

    g = sub.add_parser("daemon", help="engine daemon lifecycle")
    gs = g.add_subparsers(dest="action", required=True)
    sp = gs.add_parser("start")
    sp.add_argument("--tcp", help="also listen on host:port for remote clients")
    sp.set_defaults(f=cmd_daemon_start)
    gs.add_parser("ls").set_defaults(f=cmd_daemon_ls)
    gs.add_parser("stop").set_defaults(f=cmd_daemon_stop)
    gs.add_parser("destroy").set_defaults(f=cmd_daemon_destroy)

    args = p.parse_args()
    try:
        return args.f(args)
    except TerraError as e:
        return _err(str(e))
    except (FileNotFoundError, ConnectionRefusedError):
        hint = (
            "no engine daemon found — start one first:\n"
            "  terra daemon start\n"
            "  (or: python -c 'from terra.daemon import Daemon; Daemon().start()')\n"
            "  or point at an existing one: --socket <path|tcp://host:port> or TERRA_SOCKET"
        )
        return _err(hint)
    except PermissionError:
        return _err(
            "socket exists but is not usable by this user (owned by root?) — "
            "socket unusable by this user — start your own daemon: terra daemon start"
        )
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())

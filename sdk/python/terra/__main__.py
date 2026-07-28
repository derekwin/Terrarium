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
from .sandbox import Sandbox
from .pool import Pool


def _client(args) -> TerraClient:
    addr = args.socket
    if addr is None:
        return TerraClient()
    if addr.startswith("tcp://"):
        return TerraClient(addr, token=os.environ.get("TERRA_TOKEN"))
    return TerraClient(addr)


def _output(data, args) -> int:
    """Print *data* in human-readable or JSON format depending on --json."""
    if getattr(args, "json", False):
        print(json.dumps(data, indent=2, ensure_ascii=False))
        return 0
    _print_human(data)
    return 0


def _print(data) -> int:
    """Legacy compat: always JSON (used by old code paths)."""
    print(json.dumps(data, indent=2, ensure_ascii=False))
    return 0


def _print_human(data) -> None:
    """Print *data* in a human-readable format (key-value / list)."""
    if isinstance(data, dict):
        for k, v in data.items():
            if isinstance(v, list):
                print(f"{k}:")
                for item in v:
                    if isinstance(item, dict):
                        print(f"  {json.dumps(item, ensure_ascii=False)}")
                    else:
                        print(f"  {item}")
            elif isinstance(v, dict):
                print(f"{k}:")
                for k2, v2 in v.items():
                    print(f"  {k2}: {v2}")
            else:
                print(f"{k}: {v}")
    elif isinstance(data, list):
        for item in data:
            print(item)
    else:
        print(data)


def _err(msg: str, *, cause: str = "", fix: str = "") -> int:
    """Print a structured error with what / why / how."""
    print(f"Error: {msg}", file=sys.stderr)
    if cause:
        print(f"Cause: {cause}", file=sys.stderr)
    if fix:
        print(f"Fix:   {fix}", file=sys.stderr)
    return 1


# ---------------------------------------------------------------------------
# daemon commands
# ---------------------------------------------------------------------------
def cmd_list(args):
    return _output(_client(args).vm_list(), args)


def cmd_info(args):
    return _output(_client(args).vm_info(args.name), args)


def cmd_create(args):
    c = _client(args)
    kernel = args.kernel
    if kernel and not Path(kernel).exists():
        kernel = str(images.resolve_kernel(kernel))
    if args.layers:
        # Layered boot always uses the virtiofs bootstrap; the system
        # picks it automatically — no need to know it exists.
        rootfs = str(images.resolve_rootfs("virtiofs"))
        if args.rootfs and args.rootfs != "virtiofs":
            print("note: --rootfs ignored when --layers is given (bootstrap is automatic)")
    else:
        rootfs = args.rootfs or "alpine"
        if not Path(rootfs).exists():
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
        system=args.system,
        upper=args.upper,
        net=args.net,
    )
    return _output(resp, args)


def cmd_exec(args):
    return _output(_client(args).vm_exec(args.name, args.args, timeout_secs=args.timeout), args)


def cmd_resize(args):
    c = _client(args)
    if args.cpus is None and args.memory_bytes is None:
        return _err(
            "Nothing to resize",
            cause="Neither --cpus nor --memory-bytes was specified",
            fix="Provide at least one: --cpus N and/or --memory-bytes N",
        )
    return _output(c.vm_resize(args.name, cpus=args.cpus, memory_bytes=args.memory_bytes), args)


def _simple(method: str):
    def f(args):
        return _output(getattr(_client(args), method)(args.name), args)

    return f


def cmd_pool_create(args):
    return _output(_client(args).pool_create(args.size, kernel=args.kernel, net=args.net), args)


def cmd_pool_list(args):
    return _output(_client(args).pool_list(), args)


def cmd_pool_claim(args):
    return _output(_client(args).pool_claim(args.layers), args)


def _simple_pool_release(args):
    return _output(_client(args).pool_release(args.name), args)


def cmd_attach_fs(args):
    return _output(_client(args).vm_attach_fs(args.name, args.layers), args)


def _simple_detach(args):
    return _output(_client(args).vm_detach_fs(args.name), args)


def cmd_net_list(args):
    return _output(_client(args)._send({"command": "net_list"}), args)


def cmd_net_down(args):
    return _output(_client(args)._send({"command": "net_down"}), args)


# ---------------------------------------------------------------------------
# image commands (host-side)
# ---------------------------------------------------------------------------
def cmd_image_ls(args) -> int:
    """List all images: kernels, rootfs, initramfs."""
    kdir = paths.kernels_dir()
    rdir = paths.rootfs_dir()

    print("kernels:")
    if kdir.is_dir():
        for e in sorted(kdir.iterdir()):
            if e.is_dir() and (e / "vmlinux.bin").exists():
                print(f"  {e.name}")
    else:
        print("  (none)")

    print("rootfs:")
    seen = set()
    if rdir.is_dir():
        for e in sorted(rdir.iterdir()):
            if e.name in _INFRA_IMAGES:
                continue
            alias = _ROOTFS_ALIASES.get(e.name)
            if alias:
                if alias not in seen:
                    print(f"  {alias}")
                    seen.add(alias)
            elif e.suffix == ".cpio":
                print(f"  {e.stem}")
            elif e.name.endswith(".cpio.gz"):
                print(f"  {e.name[:-len('.cpio.gz')]}")
            else:
                print(f"  {e.name}")
    else:
        print("  (none)")

    print("initramfs:")
    if rdir.is_dir():
        for e in sorted(rdir.iterdir()):
            if e.name in _INFRA_IMAGES:
                alias = _ROOTFS_ALIASES.get(e.name)
                label = alias if alias else e.name
                print(f"  {label}")
    else:
        print("  (none)")
    return 0


def cmd_image_build_kernel(args) -> int:
    """Build a kernel image: bash images/build-kernel.sh <version> <config> <output_dir>."""
    import shutil
    import subprocess
    import tempfile

    name = args.name
    version = args.version or ""
    script = _BUILDER_SCRIPTS["kernel"]
    if not Path(script).exists():
        return _err(
            f"Build script not found: {script}",
            cause="Must be run from the Terrarium repository root",
            fix="Run this command from the repo root directory",
        )

    with tempfile.TemporaryDirectory() as td:
        r = subprocess.run(
            ["bash", script, version, "", td]
        )
        if r.returncode:
            return r.returncode
        src = Path(td) / "vmlinux.bin"
        dest = paths.kernels_dir() / name
        shutil.rmtree(dest, ignore_errors=True)
        dest.mkdir(parents=True)
        target = dest / "vmlinux.bin"
        shutil.move(str(src), str(target))
        print(f"built: {target}")
        return 0


def cmd_image_build_rootfs(args) -> int:
    """Build a bootable system rootfs. Supported: alpine, ubuntu."""
    return _build_rootfs(args)


def cmd_image_build_initramfs(args) -> int:
    """Build initramfs via terrarium_fs (Rust), replacing shell scripts."""
    import shutil
    import tempfile

    import terrarium_fs

    name = getattr(args, "name", None)
    repo = Path.cwd()
    if not (repo / "images" / "build.sh").exists():
        return _err(
            "Not in Terrarium repo root",
            cause="The images/build.sh script was not found",
            fix="Run this command from the Terrarium repository root directory",
        )

    src_rootfs = _ensure_initramfs_src_rootfs(repo)
    if args.type == "agent":
        init_template = repo / "images" / "rootfs" / "init-agent"
        gp = _ensure_initramfs_guest_proxy(repo)
        output_name = "initramfs-agent.cpio.gz"
        build_fn = lambda out: terrarium_fs.build_initramfs_agent(
            src_rootfs, gp, str(init_template), out,
        )
    else:  # virtiofs
        init_template = repo / "images" / "rootfs" / "init-virtiofs"
        output_name = "initramfs-virtiofs.cpio.gz"
        build_fn = lambda out: terrarium_fs.build_initramfs_virtiofs(
            src_rootfs, str(init_template), out,
        )

    if name:
        with tempfile.TemporaryDirectory() as td:
            out = Path(td) / output_name
            build_fn(str(out))
            dest = paths.rootfs_dir() / name
            if dest.exists():
                shutil.rmtree(dest)
            dest.mkdir(parents=True)
            shutil.move(str(out), str(dest / output_name))
            print(f"built: {dest / output_name}")
            return 0

    out = repo / "target" / "guest" / output_name
    out.parent.mkdir(parents=True, exist_ok=True)
    build_fn(str(out))
    return 0


def cmd_image_remove(args) -> int:
    """Remove any image — checks kernels_dir and rootfs_dir."""
    import shutil

    name = args.name
    # Check kernels_dir
    kpath = paths.kernels_dir() / name
    if kpath.exists():
        if kpath.is_dir():
            shutil.rmtree(kpath)
        else:
            kpath.unlink()
        print(f"removed kernel: {kpath}")
        return 0
    # Check rootfs_dir — try name directly, and with known extensions
    rdir = paths.rootfs_dir()
    for cand_name in (name, f"{name}.cpio", f"{name}.cpio.gz", f"{name}.img"):
        rpath = rdir / cand_name
        if rpath.exists():
            if rpath.is_dir():
                shutil.rmtree(rpath)
            else:
                rpath.unlink()
            print(f"removed rootfs: {rpath}")
            return 0
    # Check aliases
    for alias_img, alias_name in _ROOTFS_ALIASES.items():
        if alias_name == name:
            rpath = rdir / alias_img
            if rpath.exists():
                rpath.unlink()
                print(f"removed rootfs: {rpath}")
                return 0
    return _err(
        f"Image not found: {name}",
        cause="No kernel or rootfs image with this name exists",
        fix="List available images: terra image ls",
    )


def cmd_daemon_start(args):
    """Start a daemon as a detached background process.

    Spawns a Python subprocess that calls Daemon.start(), which runs
    the engine in a Rust background thread via PyO3 FFI.
    """
    import subprocess

    existing = _daemon_pids()
    if existing:
        print(f"daemon already running (pid={existing[0]}) — stop it first: terra daemon stop")
        return 1

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


def _build_rootfs(args) -> int:
    """Internal: create a bootable system rootfs. Supported: alpine, ubuntu."""
    import subprocess

    name = args.name
    if name == "ubuntu":
        layer_dir = Path(os.environ.get("TERRA_LAYER_DIR") or paths.layers_dir()) / "ubuntu"
        if not layer_dir.is_dir():
            r = subprocess.run(["bash", "images/build-layer-distro.sh", "ubuntu"])
            if r.returncode:
                return r.returncode
        return _pack_layer_as_rootfs("ubuntu", "ubuntu")
    if name == "alpine":
        img = images.ensure("alpine.cpio")
        print(f"rootfs ready: {img}")
        return 0
    return _err(
        f"Unsupported rootfs: {name!r}",
        cause="Only 'alpine' (musl) and 'ubuntu' (glibc) are supported",
        fix=f"Use --name alpine or --name ubuntu",
    )


def _pack_layer_as_rootfs(layer_name: str, out_name: str) -> int:
    """Pack a layer directory into a bootable rootfs cpio image."""
    import terrarium_fs

    layer_dir = str(Path(os.environ.get("TERRA_LAYER_DIR") or paths.layers_dir()) / layer_name)
    output_dir = str(paths.rootfs_dir())
    try:
        out_path = terrarium_fs.pack_cpio_rootfs(layer_dir, out_name, output_dir)
        print(f"rootfs image built: {out_path}")
        return 0
    except Exception as e:
        return _err(str(e))


def cmd_image_base(args):
    """Build/refresh the base layer in the managed layers dir.

    Extracts the managed alpine rootfs into layers/<name> (default:
    "base") so `layers=["base"]` resolves with zero env setup.
    """
    import shutil
    import terrarium_fs

    name = args.name
    dest = paths.layers_dir() / name
    if dest.exists() and not args.force:
        print(f"{dest} exists (use --force to rebuild)")
        return 0
    shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True)
    cpio = images.ensure("alpine.cpio")
    try:
        terrarium_fs.extract_cpio_layer(str(cpio), str(dest))
    except Exception as e:
        return _err(str(e))
    print(f"base layer ready: {dest}")
    return 0


# Names that are system bases, not add-on layers — they belong to the
# rootfs namespace and are hidden from `layer ls`.
_SYSTEM_LAYER_NAMES = {"base", "ubuntu", ".system"}


def cmd_image_layers(args):
    import terrarium_fs

    layer_dir = os.environ.get("TERRA_LAYER_DIR") or str(paths.layers_dir())
    try:
        for name in terrarium_fs.list_layers(layer_dir):
            print(name)
        return 0
    except Exception as e:
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
}


def _ensure_initramfs_src_rootfs(repo: Path) -> str:
    """Return a directory with bin/busybox and musl libs."""
    import subprocess
    import tempfile

    rootfs_dir = repo / "target" / "guest" / "rootfs"
    if (rootfs_dir / "bin" / "busybox").exists():
        return str(rootfs_dir)

    alpine = repo / "target" / "guest" / "alpine.cpio"
    if not alpine.exists():
        raise FileNotFoundError(
            f"no rootfs source: need {rootfs_dir} or {alpine} "
            f"(run build-rootfs.sh first)"
        )

    extract_dir = tempfile.mkdtemp(prefix="terrarium-src-")
    cmd = (
        f"zcat '{alpine}' 2>/dev/null || cat '{alpine}'"
        f" | (cd '{extract_dir}' && cpio -idm --quiet)"
    )
    subprocess.run(cmd, shell=True, check=True)
    return extract_dir


def _ensure_initramfs_guest_proxy(repo: Path) -> str:
    """Return the guest-proxy binary path; build it if missing."""
    import subprocess

    gp = repo / "target" / "x86_64-unknown-linux-musl" / "release" / "guest-proxy"
    if not gp.exists():
        subprocess.run(
            [
                "cargo", "build", "--release",
                "--target", "x86_64-unknown-linux-musl",
                "-p", "guest-proxy",
            ],
            cwd=repo, check=True,
        )
    return str(gp)


def cmd_image_layer_build(args):
    """Build a tool layer by configuring inside a builder VM."""
    # Preflight: networked builds need a privileged daemon — offer the
    # two ways out before burning a builder VM.
    if not args.no_net and os.geteuid() != 0:
        return _err(
            "Networked builder VM requires root privileges",
            cause="This build uses a networked builder VM (downloads), which needs "
            "CAP_NET_ADMIN for tap device creation",
            fix='sudo env "PATH=$PATH" terra daemon start  (then retry)\n'
            "     or: add --no-net for an offline build",
        )
    system = {"alpine": "base", "ubuntu": "ubuntu"}.get(args.rootfs)
    if system is None:
        return _err(
            f"Unsupported rootfs: {args.rootfs!r}",
            cause="Only 'alpine' (musl) and 'ubuntu' (glibc) are supported",
            fix="Use --rootfs alpine or --rootfs ubuntu",
        )
    client = _client(args)
    name = args.name
    builder = f"lb-{name}"

    # 1) builder VM from the base layer, persistent upper
    try:
        client.vm_create(
            builder,
            args.kernel,
            initramfs=str(images.resolve_rootfs("virtiofs")),
            cpus=1,
            memory_mb=512,
            layers=[system],
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


_ROOTFS_ALIASES = {
    "alpine.cpio": "alpine",
    "initramfs-agent.cpio.gz": "agent",
    "initramfs-virtiofs.cpio.gz": "virtiofs",
}

# Infra bootstrap images: needed by the system, not user-facing.
_INFRA_IMAGES = {"initramfs-agent.cpio.gz", "initramfs-virtiofs.cpio.gz"}


def _rootfs_variants() -> list[str]:
    """Logical names users type: --rootfs <name> (infra images hidden)."""
    out = []
    for e in sorted(paths.rootfs_dir().iterdir()):
        if e.name in _INFRA_IMAGES:
            continue
        if e.name in _ROOTFS_ALIASES:
            out.append(_ROOTFS_ALIASES[e.name])
        elif e.suffix == ".cpio":
            out.append(e.stem)
        elif e.name.endswith(".cpio.gz"):
            out.append(e.name[: -len(".cpio.gz")])
        else:
            out.append(e.name)
    return out


def _kernel_list(args) -> int:
    for e in sorted(paths.kernels_dir().iterdir()):
        if e.is_dir() and (e / "vmlinux.bin").exists():
            print(e.name)
    return 0


def _rootfs_list(args) -> int:
    for line in _rootfs_variants():
        print(line)
    return 0


def _remove_path(path: Path) -> int:
    import shutil

    if not path.exists():
        return _err(
            f"Not found: {path}",
            cause="The specified resource does not exist",
            fix="List available items with the corresponding 'ls' command",
        )
    if path.is_dir():
        shutil.rmtree(path)
    else:
        path.unlink()
    print(f"removed {path}")
    return 0


def _kernel_remove(args) -> int:
    return _remove_path(paths.kernels_dir() / args.name)


def _rootfs_remove(args) -> int:
    return _remove_path(paths.rootfs_dir() / args.name)


def cmd_layer_remove(args):
    import terrarium_fs

    layer_dir = os.environ.get("TERRA_LAYER_DIR") or str(paths.layers_dir())
    try:
        terrarium_fs.remove_layer(args.name, layer_dir)
        print(f"removed layer '{args.name}'")
        return 0
    except Exception as e:
        return _err(str(e))


def cmd_layer_create(args):
    if args.from_dir:
        out = images.build_layer(args.from_dir, args.name)
        print(f"layer built: {out}")
        return 0
    if args.from_image:
        args2 = argparse.Namespace(name=args.name, force=True)
        return cmd_image_base(args2)
    # --script path (build-by-doing) requires kernel; initramfs auto-resolved
    if not args.kernel:
        return _err(
            "Missing --kernel for script-based layer build",
            cause="--script needs a builder VM, which requires a kernel",
            fix="Provide --kernel (e.g. --kernel k612)",
        )
    return cmd_image_layer_build(args)


def cmd_net_create(args):
    return _output(_client(args)._send({"command": "net_up"}), args)


def _daemon_pidfile() -> Path:
    return paths.run_dir() / "daemon.pid"


def cmd_daemon_ls(args):
    import json as _json

    info = {"socket": paths.default_socket()}
    pids = _daemon_pids()
    info["pids"] = pids
    info["alive"] = bool(pids) or Path(info["socket"]).exists()
    print(_json.dumps(info, indent=2))
    return 0


def _daemon_pids() -> list[int]:
    """Live daemon pids (zombies excluded) — any start method."""
    pidfile = _daemon_pidfile()
    if pidfile.exists():
        try:
            pid = int(pidfile.read_text().strip())
            stat = Path(f"/proc/{pid}/stat").read_text().split()[2]
            if stat != "Z":
                return [pid]
        except (OSError, ValueError):
            pass
    import subprocess

    out = subprocess.run(["pgrep", "-x", "engine"], capture_output=True, text=True)
    pids = []
    for p in out.stdout.split():
        if not p.strip().isdigit():
            continue
        try:
            stat = Path(f"/proc/{p}/stat").read_text().split()[2]
            if stat != "Z":
                pids.append(int(p))
        except OSError:
            pass
    return pids


def _daemon_stop(sig) -> int:
    import signal as _signal

    pids = _daemon_pids()
    if not pids:
        _daemon_pidfile().unlink(missing_ok=True)
        return _err(
            "No engine daemon running",
            cause="No daemon process or PID file found",
            fix="If you want to ensure a clean state, try: terra daemon destroy",
        )
    for pid in pids:
        os.kill(pid, sig)
        print(f"sent {_signal.Signals(sig).name} to daemon (pid={pid})")
    _daemon_pidfile().unlink(missing_ok=True)
    return 0


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


def cmd_daemon_config(args):
    """Composed live view: engine, pool, network."""
    import json as _json

    c = _client(args)
    out = {}
    pids = _daemon_pids()
    out["engine"] = {"pids": pids, "socket": paths.default_socket()}
    try:
        pool = c.pool_list()
        out["pool"] = {
            "size": pool.get("count", 0),
            "claimed": sum(1 for s in pool.get("pool", []) if s.get("claimed")),
        }
    except TerraError:
        out["pool"] = "unavailable"
    try:
        out["net"] = c._send({"command": "net_list"})
    except TerraError:
        out["net"] = "unavailable"
    out["layers_dir"] = str(paths.layers_dir())
    out["layers"] = [e.name for e in sorted(paths.layers_dir().iterdir())] if paths.layers_dir().is_dir() else []
    print(_json.dumps(out, indent=2, ensure_ascii=False))
    return 0


# ---------------------------------------------------------------------------
# sandbox commands (high-level unified API)
# ---------------------------------------------------------------------------
def cmd_sandbox_create(args):
    """Create a sandbox using the high-level SDK."""
    try:
        sb = Sandbox(
            template=args.template,
            layers=args.layers or None,
            kernel=args.kernel,
            cpu=args.cpu,
            memory_mb=args.memory,
            network=bool(args.network),
            timeout=args.timeout,
        )
        return _output(
            {
                "name": sb.id,
                "status": sb.status,
                "backend": sb.backend,
            },
            args,
        )
    except Exception as e:
        return _err(str(e))


def cmd_sandbox_ls(args):
    """List running sandboxes."""
    c = _client(args)
    try:
        vms = c.vm_list()
        sandbox_vms = [
            v for v in vms.get("vms", []) if v.get("name", "").startswith("sandbox-")
        ]
        return _output(
            {"sandboxes": sandbox_vms, "count": len(sandbox_vms)},
            args,
        )
    except TerraError as e:
        return _err(str(e))


# ---------------------------------------------------------------------------
def main() -> int:
    p = argparse.ArgumentParser(
        prog="terra",
        description="Terrarium CLI (python -m terra)",
        epilog="""\
Common workflows:
  First time setup:
    terra image build kernel -n k612 --version 6.12
    terra image build rootfs -n alpine

  Quick sandbox:
    terra sandbox create --template python312

  Warm pool:
    terra pool create --size 3
    terra pool claim --layers python312 base

  Direct VM:
    terra vm create dev --kernel k612 --layers base --net
    terra vm exec dev -- python3 --version
    terra vm remove dev
""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--socket", help="daemon socket path or tcp://host:port")
    p.add_argument("--json", action="store_true", help="machine-readable JSON output")
    sub = p.add_subparsers(dest="cmd", required=True)

    # --- unified resource groups: vm/image/layer/pool/net/daemon ---
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
    sp.add_argument("--system", help="system base layer (default: base)")
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

    sp = vms.add_parser("attach", help="hot-plug layers to a running VM")
    sp.add_argument("name")
    sp.add_argument("--layers", nargs="+", required=True)
    sp.set_defaults(f=cmd_attach_fs)
    sp = vms.add_parser("detach", help="hot-unplug layers from a running VM")
    sp.add_argument("name")
    sp.set_defaults(f=_simple_detach)
    # Backward-compat hidden aliases
    sp = vms.add_parser("attach-fs", help=argparse.SUPPRESS)
    sp.add_argument("name")
    sp.add_argument("--layers", nargs="+", required=True)
    sp.set_defaults(f=cmd_attach_fs)
    sp = vms.add_parser("detach-fs", help=argparse.SUPPRESS)
    sp.add_argument("name")
    sp.set_defaults(f=_simple_detach)

    # --- image: unified guest images (kernels, rootfs, initramfs) ---
    g = sub.add_parser("image", help="manage guest images (kernel, rootfs, initramfs)")
    gs = g.add_subparsers(dest="action", required=True)
    gs.add_parser("ls").set_defaults(f=cmd_image_ls)

    b = gs.add_parser("build")
    bs = b.add_subparsers(dest="what", required=True)

    k = bs.add_parser("kernel")
    k.add_argument("-n", "--name", required=True)
    k.add_argument("--version", default="6.12")
    k.set_defaults(f=cmd_image_build_kernel)

    r = bs.add_parser("rootfs")
    r.add_argument("-n", "--name", required=True, choices=["alpine", "ubuntu"])
    r.set_defaults(f=cmd_image_build_rootfs)

    i = bs.add_parser("initramfs")
    i.add_argument("--type", required=True, choices=["virtiofs", "agent"])
    i.add_argument("-n", "--name", help="variant name (optional)")
    i.set_defaults(f=cmd_image_build_initramfs)

    rm = gs.add_parser("remove")
    rm.add_argument("-n", "--name", required=True)
    rm.set_defaults(f=cmd_image_remove)

    g = sub.add_parser("layer", help="manage filesystem layers")
    gs = g.add_subparsers(dest="action", required=True)
    gs.add_parser("ls").set_defaults(f=cmd_image_layers)
    c = gs.add_parser("create")
    c.add_argument("-n", "--name", required=True)
    src = c.add_mutually_exclusive_group(required=True)
    src.add_argument("--from-dir", help="pack an existing directory")
    src.add_argument("--script", help="build-by-doing: run setup in a builder VM")
    src.add_argument("--from-image", action="store_true", help="base layer from guest rootfs")
    c.add_argument("--rootfs", required=True,
                   help="system the layer is built on: alpine (musl) or ubuntu (glibc)")
    c.add_argument("--kernel", help="kernel for builder VM (required with --script)")

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
    gs.add_parser("config", help="show live daemon state").set_defaults(f=cmd_daemon_config)
    gs.add_parser("stop").set_defaults(f=cmd_daemon_stop)
    gs.add_parser("destroy").set_defaults(f=cmd_daemon_destroy)

    g = sub.add_parser("sandbox", help="high-level sandbox operations (recommended)")
    gs = g.add_subparsers(dest="action", required=True)
    c = gs.add_parser("create")
    c.add_argument("--template", help="template name")
    c.add_argument("--layers", nargs="*")
    c.add_argument("--kernel")
    c.add_argument("--cpu", type=int, default=1)
    c.add_argument("--memory", type=int, default=256)
    c.add_argument("--network", action="store_true")
    c.add_argument("--timeout", type=int, default=600)
    c.set_defaults(f=cmd_sandbox_create)
    gs.add_parser("ls").set_defaults(f=cmd_sandbox_ls)

    args = p.parse_args()
    try:
        return args.f(args)
    except TerraError as e:
        return _err(str(e))
    except (FileNotFoundError, ConnectionRefusedError):
        return _err(
            "Cannot connect to engine daemon",
            cause=f"No daemon running at {paths.default_socket()}",
            fix="Run 'terra daemon start' to start the engine",
        )
    except PermissionError:
        return _err(
            "Socket permission denied",
            cause=f"Socket at {paths.default_socket()} is owned by another user (root?)",
            fix="Start your own daemon: terra daemon start",
        )
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())

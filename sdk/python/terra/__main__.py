"""terra — Terrarium CLI (Python).

    python -m terra <command> [args]
    # or, once pip-installed: terra <command> [args]

Admin/user command line — daemon operations go through the engine
socket (TERRA_SOCKET or managed default); host-side image operations
run locally.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import shutil
import signal as _signal
import subprocess
import sys
import time
import tempfile
from pathlib import Path

from . import images, paths
from .client import TerraClient, TerraError
from .sandbox import Sandbox
from .template import Template

# ═══════════════════════════════════════════════════════════════════
# Exit codes
# ═══════════════════════════════════════════════════════════════════
EXIT_OK = 0
EXIT_ERROR = 1
EXIT_USAGE = 2
EXIT_DAEMON = 3
EXIT_NOTFOUND = 4
EXIT_TIMEOUT = 5


# ═══════════════════════════════════════════════════════════════════
# Helpers
# ═══════════════════════════════════════════════════════════════════

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
        return EXIT_OK
    if getattr(args, "verbose", False) and isinstance(data, dict):
        print(json.dumps(data, indent=2, ensure_ascii=False))
        return EXIT_OK
    _print_human(data)
    return EXIT_OK


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


def _err(msg: str, *, cause: str = "", fix: str = "", exit_code: int = EXIT_ERROR) -> int:
    """Print a structured error with what / why / how and return *exit_code*."""
    print(f"Error: {msg}", file=sys.stderr)
    if cause:
        print(f"Cause: {cause}", file=sys.stderr)
    if fix:
        print(f"Fix:   {fix}", file=sys.stderr)
    return exit_code


def _parse_multi(values: list[str] | None) -> list[str]:
    """Parse comma-separated multi-value args: ['a,b', 'c'] → ['a','b','c']."""
    if not values:
        return []
    result: list[str] = []
    for v in values:
        for part in v.split(","):
            part = part.strip()
            if part:
                result.append(part)
    return result


def _parse_kv_pairs(values: list[str] | None) -> dict[str, str]:
    """Parse KEY=VALUE pairs into a dict."""
    if not values:
        return {}
    result: dict[str, str] = {}
    for v in values:
        if "=" in v:
            k, val = v.split("=", 1)
            result[k.strip()] = val.strip()
        else:
            result[v.strip()] = ""
    return result


# ═══════════════════════════════════════════════════════════════════
# sandbox commands (high-level unified API)
# ═══════════════════════════════════════════════════════════════════

def cmd_sandbox_create(args) -> int:
    """Create a sandbox using the high-level SDK."""
    try:
        sb = Sandbox(
            template=args.template,
            layers=args.layers or None,
            kernel=args.kernel,
            cpu=args.cpu,
            memory_mb=args.memory,
            network=bool(args.net),
            env=_parse_kv_pairs(args.env),
            timeout=args.timeout,
        )
        return _output(
            {
                "id": sb.id,
                "name": sb.id,
                "status": sb.status,
                "backend": sb.backend,
            },
            args,
        )
    except FileNotFoundError as e:
        return _err(str(e), exit_code=EXIT_NOTFOUND)
    except Exception as e:
        return _err(str(e))


def cmd_sandbox_ls(args) -> int:
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


def cmd_sandbox_info(args) -> int:
    """Show details about a specific sandbox."""
    c = _client(args)
    try:
        info = c.vm_info(args.id)
        return _output(info, args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_sandbox_exec(args) -> int:
    """Execute a command inside a sandbox."""
    c = _client(args)
    try:
        cmd_args = args.args
        if not cmd_args:
            return _err("No command specified", fix="Usage: terra sandbox exec ID -- COMMAND...", exit_code=EXIT_USAGE)

        # Inject cwd / env via shell wrapping
        prefix_parts: list[str] = []
        if args.cwd and args.cwd != "/workdir":
            prefix_parts.append(f"cd {args.cwd}")
        if args.env:
            for k, v in _parse_kv_pairs(args.env).items():
                prefix_parts.append(f"export {k}={v}")
        if prefix_parts:
            inner = " ".join(cmd_args)
            cmd_args = ["sh", "-c", " && ".join(prefix_parts + [inner])]

        resp = c.vm_exec(args.id, cmd_args, timeout_secs=args.timeout)
        return _output(resp, args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        if "timeout" in msg.lower():
            return _err(msg, exit_code=EXIT_TIMEOUT)
        return _err(msg)


def cmd_sandbox_cp(args) -> int:
    """Copy files between host and sandbox (docker cp style)."""
    src = args.src
    dst = args.dst
    c = _client(args)

    def _is_remote(p: str) -> bool:
        return ":" in p and not p.startswith("/") and not p.startswith(".")

    src_remote = _is_remote(src)
    dst_remote = _is_remote(dst)

    if src_remote and dst_remote:
        return _err(
            "Both src and dst are remote — sandbox-to-sandbox copy not supported",
            fix="One of src/dst must be a local path",
            exit_code=EXIT_USAGE,
        )
    if not src_remote and not dst_remote:
        return _err(
            "Both src and dst are local — use cp directly",
            exit_code=EXIT_USAGE,
        )

    try:
        if src_remote:
            # Download: sandbox:/path → local
            sandbox_id, remote_path = src.split(":", 1)
            resp = c.vm_exec(sandbox_id, ["cat", remote_path], timeout_secs=30)
            content = resp.get("stdout", "")
            with open(dst, "w") as f:
                f.write(content)
            print(f"Downloaded {src} → {dst}")
        else:
            # Upload: local → sandbox:/path
            sandbox_id, remote_path = dst.split(":", 1)
            with open(src, "rb") as f:
                data = base64.b64encode(f.read()).decode()
            c.vm_exec(sandbox_id, ["sh", "-c", f"echo {data} | base64 -d > {remote_path}"], timeout_secs=30)
            print(f"Uploaded {src} → {dst}")
        return EXIT_OK
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)
    except FileNotFoundError as e:
        return _err(str(e), exit_code=EXIT_NOTFOUND)


def cmd_sandbox_resize(args) -> int:
    """Resize sandbox resources online."""
    c = _client(args)
    try:
        kwargs: dict = {}
        if args.cpu is not None:
            kwargs["cpus"] = args.cpu
        if args.memory is not None:
            kwargs["memory_bytes"] = args.memory * 1024 * 1024
        if not kwargs:
            return _err(
                "Nothing to resize",
                cause="Neither --cpu nor --memory was specified",
                fix="Provide at least one: --cpu N and/or --memory MB",
                exit_code=EXIT_USAGE,
            )
        resp = c.vm_resize(args.id, **kwargs)
        return _output(resp, args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_sandbox_metrics(args) -> int:
    """Query sandbox resource usage."""
    c = _client(args)
    try:
        info = c.vm_info(args.id)
        return _output(
            {
                "id": args.id,
                "cpu_count": info.get("cpus"),
                "memory_mb": info.get("memory_mb"),
                "state": info.get("state"),
            },
            args,
        )
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_sandbox_kill(args) -> int:
    """Force-kill and deregister a sandbox."""
    c = _client(args)
    try:
        resp = c.vm_destroy(args.id)
        return _output(resp, args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


# ═══════════════════════════════════════════════════════════════════
# template commands
# ═══════════════════════════════════════════════════════════════════

def cmd_template_ls(args) -> int:
    """List saved templates."""
    try:
        names = Template.list()
        if not names:
            print("(no templates)")
            return EXIT_OK
        return _output(names, args)
    except Exception as e:
        return _err(str(e))


def cmd_template_create(args) -> int:
    """Create a template from kernel + rootfs + layers."""
    try:
        base = args.rootfs
        if base not in ("alpine", "ubuntu"):
            return _err(
                f"Unsupported rootfs: {base!r}",
                cause="Only 'alpine' (musl) and 'ubuntu' (glibc) are supported",
                fix="Use --rootfs alpine or --rootfs ubuntu",
                exit_code=EXIT_USAGE,
            )
        layers = _parse_multi(args.layers) if args.layers else []
        t = Template(
            name=args.name,
            base=base,
            layers=layers,
            kernel=args.kernel or None,
        )
        path = t.save()
        return _output(
            {"name": t.name, "base": t.base, "layers": t.layers, "kernel": t.kernel, "path": str(path)},
            args,
        )
    except Exception as e:
        return _err(str(e))


def cmd_template_info(args) -> int:
    """Show template details."""
    try:
        t = Template.load(args.name)
        return _output(
            {"name": t.name, "base": t.base, "layers": t.layers, "kernel": t.kernel},
            args,
        )
    except FileNotFoundError:
        return _err(
            f"Template not found: {args.name!r}",
            cause="No template with this name exists",
            fix="List templates: terra template ls",
            exit_code=EXIT_NOTFOUND,
        )
    except Exception as e:
        return _err(str(e))


def cmd_template_remove(args) -> int:
    """Remove a template."""
    try:
        removed = Template.remove(args.name)
        if removed:
            print(f"removed template '{args.name}'")
            return EXIT_OK
        return _err(
            f"Template not found: {args.name!r}",
            exit_code=EXIT_NOTFOUND,
        )
    except Exception as e:
        return _err(str(e))


# ═══════════════════════════════════════════════════════════════════
# image commands
# ═══════════════════════════════════════════════════════════════════

# Rootfs aliases for display/resolution.
_ROOOTFS_ALIASES = {
    "alpine.cpio": "alpine",
    "initramfs-agent.cpio.gz": "agent",
    "initramfs-virtiofs.cpio.gz": "virtiofs",
}

# Infra bootstrap images: needed by the system, not user-facing.
_INFRA_IMAGES = {"initramfs-agent.cpio.gz", "initramfs-virtiofs.cpio.gz"}

# Names that are system bases, not add-on layers.
_SYSTEM_LAYER_NAMES = {"base", "ubuntu", ".system"}


def cmd_image_ls(args) -> int:
    """List all images: kernels, rootfs, initramfs."""
    kdir = paths.kernels_dir()
    rdir = paths.rootfs_dir()

    if args.json or getattr(args, "verbose", False):
        result: dict = {"kernels": [], "rootfs": [], "initramfs": []}
        if kdir.is_dir():
            for e in sorted(kdir.iterdir()):
                if e.is_dir() and (e / "vmlinux.bin").exists():
                    result["kernels"].append(e.name)
        seen_rootfs = set()
        if rdir.is_dir():
            for e in sorted(rdir.iterdir()):
                if e.name in _INFRA_IMAGES:
                    alias = _ROOOTFS_ALIASES.get(e.name)
                    if alias and alias not in seen_rootfs:
                        result["initramfs"].append(alias)
                        seen_rootfs.add(alias)
                elif e.name in _ROOOTFS_ALIASES:
                    alias = _ROOOTFS_ALIASES[e.name]
                    if alias not in seen_rootfs:
                        result["rootfs"].append(alias)
                        seen_rootfs.add(alias)
                else:
                    if e.suffix == ".cpio":
                        result["rootfs"].append(e.stem)
                    elif e.name.endswith(".cpio.gz"):
                        result["rootfs"].append(e.name[: -len(".cpio.gz")])
                    else:
                        result["rootfs"].append(e.name)
        return _output(result, args)

    # Human-readable output
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
            alias = _ROOOTFS_ALIASES.get(e.name)
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
                alias = _ROOOTFS_ALIASES.get(e.name)
                label = alias if alias else e.name
                print(f"  {label}")
    else:
        print("  (none)")
    return EXIT_OK


def cmd_image_build_kernel(args) -> int:
    """Build a kernel image: bash images/build-kernel.sh <version> <config> <output_dir>."""
    name = args.name
    version = args.version or ""
    script = Path("images/build-kernel.sh")
    if not script.exists():
        return _err(
            f"Build script not found: {script}",
            cause="Must be run from the Terrarium repository root",
            fix="Run this command from the repo root directory",
        )

    with tempfile.TemporaryDirectory() as td:
        r = subprocess.run(
            ["bash", str(script), version, "", td]
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
        return EXIT_OK


def cmd_image_build_rootfs(args) -> int:
    """Build a bootable system rootfs. Supported: alpine, ubuntu."""
    return _build_rootfs(args)


def cmd_image_build_initramfs(args) -> int:
    """Build initramfs via terrarium_fs (Rust), replacing shell scripts."""
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
        def build_fn(out: str) -> None:
            terrarium_fs.build_initramfs_agent(
                src_rootfs, gp, str(init_template), out,
            )
    else:  # virtiofs
        init_template = repo / "images" / "rootfs" / "init-virtiofs"
        output_name = "initramfs-virtiofs.cpio.gz"
        def build_fn(out: str) -> None:
            terrarium_fs.build_initramfs_virtiofs(
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
            return EXIT_OK

    out = repo / "target" / "guest" / output_name
    out.parent.mkdir(parents=True, exist_ok=True)
    build_fn(str(out))
    return EXIT_OK


def cmd_image_info(args) -> int:
    """Show details about a specific image (kernel or rootfs)."""
    name = args.name

    # Check kernels_dir
    kpath = paths.kernels_dir() / name
    if kpath.is_dir() and (kpath / "vmlinux.bin").exists():
        vmlinux = kpath / "vmlinux.bin"
        stat = vmlinux.stat()
        return _output(
            {
                "type": "kernel",
                "name": name,
                "path": str(vmlinux),
                "size_bytes": stat.st_size,
                "mtime": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(stat.st_mtime)),
            },
            args,
        )

    # Check rootfs_dir
    rdir = paths.rootfs_dir()
    for cand_name in (name, f"{name}.cpio", f"{name}.cpio.gz", f"{name}.img"):
        rpath = rdir / cand_name
        if rpath.exists():
            stat = rpath.stat()
            return _output(
                {
                    "type": "rootfs",
                    "name": name,
                    "filename": cand_name,
                    "path": str(rpath),
                    "size_bytes": stat.st_size,
                    "mtime": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(stat.st_mtime)),
                },
                args,
            )

    # Check aliases
    for alias_img, alias_name in _ROOOTFS_ALIASES.items():
        if alias_name == name:
            rpath = rdir / alias_img
            if rpath.exists():
                stat = rpath.stat()
                return _output(
                    {
                        "type": "initramfs",
                        "name": name,
                        "filename": alias_img,
                        "path": str(rpath),
                        "size_bytes": stat.st_size,
                        "mtime": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(stat.st_mtime)),
                    },
                    args,
                )

    return _err(
        f"Image not found: {name!r}",
        cause="No kernel or rootfs image with this name exists",
        fix="List available images: terra image ls",
        exit_code=EXIT_NOTFOUND,
    )


def cmd_image_remove(args) -> int:
    """Remove any image — checks kernels_dir and rootfs_dir."""
    name = args.name
    # Check kernels_dir
    kpath = paths.kernels_dir() / name
    if kpath.exists():
        if kpath.is_dir():
            shutil.rmtree(kpath)
        else:
            kpath.unlink()
        print(f"removed kernel: {kpath}")
        return EXIT_OK
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
            return EXIT_OK
    # Check aliases
    for alias_img, alias_name in _ROOOTFS_ALIASES.items():
        if alias_name == name:
            rpath = rdir / alias_img
            if rpath.exists():
                rpath.unlink()
                print(f"removed rootfs: {rpath}")
                return EXIT_OK
    return _err(
        f"Image not found: {name!r}",
        cause="No kernel or rootfs image with this name exists",
        fix="List available images: terra image ls",
        exit_code=EXIT_NOTFOUND,
    )


# ── internal build helpers ───────────────────────────────────────

def _build_rootfs(args) -> int:
    """Internal: create a bootable system rootfs. Supported: alpine, ubuntu."""
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
        return EXIT_OK
    return _err(
        f"Unsupported rootfs: {name!r}",
        cause="Only 'alpine' (musl) and 'ubuntu' (glibc) are supported",
        fix="Use --name alpine or --name ubuntu",
        exit_code=EXIT_USAGE,
    )


def _pack_layer_as_rootfs(layer_name: str, out_name: str) -> int:
    """Pack a layer directory into a bootable rootfs cpio image."""
    import terrarium_fs

    layer_dir = str(Path(os.environ.get("TERRA_LAYER_DIR") or paths.layers_dir()) / layer_name)
    output_dir = str(paths.rootfs_dir())
    try:
        out_path = terrarium_fs.pack_cpio_rootfs(layer_dir, out_name, output_dir)
        print(f"rootfs image built: {out_path}")
        return EXIT_OK
    except Exception as e:
        return _err(str(e))


def _ensure_initramfs_src_rootfs(repo: Path) -> str:
    """Return a directory with bin/busybox and musl libs."""
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


# ═══════════════════════════════════════════════════════════════════
# layer commands
# ═══════════════════════════════════════════════════════════════════

def cmd_layer_ls(args) -> int:
    """List filesystem layers."""
    import terrarium_fs

    layer_dir = os.environ.get("TERRA_LAYER_DIR") or str(paths.layers_dir())
    try:
        names = terrarium_fs.list_layers(layer_dir)
        return _output(list(names), args)
    except Exception as e:
        return _err(f"read {layer_dir}: {e}")


def cmd_layer_create(args) -> int:
    """Create a filesystem layer."""
    if args.from_dir:
        out = images.build_layer(args.from_dir, args.name)
        print(f"layer built: {out}")
        return EXIT_OK

    if args.from_image:
        return _build_layer_from_image(args)

    # --script path (build-by-doing) requires kernel; initramfs auto-resolved
    if not args.kernel:
        return _err(
            "Missing --kernel for script-based layer build",
            cause="--script needs a builder VM, which requires a kernel",
            fix="Provide --kernel (e.g. --kernel k612)",
            exit_code=EXIT_USAGE,
        )
    return _build_layer_via_vm(args)


def _build_layer_from_image(args) -> int:
    """Build/refresh the base layer from guest rootfs."""
    import terrarium_fs

    name = args.name
    dest = paths.layers_dir() / name
    if dest.exists():
        print(f"{dest} exists (use --overwrite to rebuild)")
        return EXIT_OK
    shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True)
    cpio = images.ensure("alpine.cpio")
    try:
        terrarium_fs.extract_cpio_layer(str(cpio), str(dest))
    except Exception as e:
        return _err(str(e))
    print(f"base layer ready: {dest}")
    return EXIT_OK


def _build_layer_via_vm(args) -> int:
    """Build a tool layer by configuring inside a builder VM."""
    system_map = {"alpine": "base", "ubuntu": "ubuntu"}
    system = system_map.get(args.rootfs)
    if system is None:
        return _err(
            f"Unsupported rootfs: {args.rootfs!r}",
            cause="Only 'alpine' (musl) and 'ubuntu' (glibc) are supported",
            fix="Use --rootfs alpine or --rootfs ubuntu",
            exit_code=EXIT_USAGE,
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

    # 2) run setup inside
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
    return EXIT_OK


def cmd_layer_info(args) -> int:
    """Show layer details."""
    layer_dir = os.environ.get("TERRA_LAYER_DIR") or str(paths.layers_dir())
    layer_path = Path(layer_dir) / args.name
    erofs_path = Path(layer_dir) / f"{args.name}.erofs"

    if layer_path.is_dir():
        # Count files (lightweight)
        file_count = sum(1 for _ in layer_path.rglob("*") if _.is_file())
        return _output(
            {
                "name": args.name,
                "type": "directory",
                "path": str(layer_path),
                "file_count": file_count,
            },
            args,
        )
    if erofs_path.exists():
        stat = erofs_path.stat()
        return _output(
            {
                "name": args.name,
                "type": "erofs",
                "path": str(erofs_path),
                "size_bytes": stat.st_size,
                "mtime": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(stat.st_mtime)),
            },
            args,
        )

    return _err(
        f"Layer not found: {args.name!r}",
        cause="No directory or .erofs image with this name",
        fix="List layers: terra layer ls",
        exit_code=EXIT_NOTFOUND,
    )


def cmd_layer_remove(args) -> int:
    """Remove a filesystem layer."""
    import terrarium_fs

    layer_dir = os.environ.get("TERRA_LAYER_DIR") or str(paths.layers_dir())
    try:
        terrarium_fs.remove_layer(args.name, layer_dir)
        print(f"removed layer '{args.name}'")
        return EXIT_OK
    except Exception as e:
        return _err(str(e))


# ═══════════════════════════════════════════════════════════════════
# vm commands
# ═══════════════════════════════════════════════════════════════════

def cmd_vm_ls(args) -> int:
    """List all VMs."""
    return _output(_client(args).vm_list(), args)


def cmd_vm_info(args) -> int:
    """Show VM details."""
    try:
        return _output(_client(args).vm_info(args.name), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_vm_create(args) -> int:
    """Create a new VM."""
    c = _client(args)
    kernel = args.kernel
    if kernel and not Path(kernel).exists():
        kernel = str(images.resolve_kernel(kernel))
    layers = _parse_multi(args.layers) if args.layers else []
    if layers:
        rootfs = str(images.resolve_rootfs("virtiofs"))
        if args.rootfs and args.rootfs != "virtiofs":
            print("note: --rootfs ignored when --layers is given (bootstrap is automatic)")
    else:
        rootfs = args.rootfs or "alpine"
        if not Path(rootfs).exists():
            rootfs = str(images.resolve_rootfs(rootfs))
    try:
        resp = c.vm_create(
            args.name,
            kernel,
            initramfs=rootfs,
            cpus=args.cpus,
            max_cpus=args.max_cpus,
            memory_mb=args.memory,
            max_memory_mb=args.max_memory,
            layers=layers or None,
            system=args.system,
            upper=args.upper,
            net=args.net,
        )
        return _output(resp, args)
    except TerraError as e:
        return _err(str(e))


def cmd_vm_exec(args) -> int:
    """Execute a command inside a VM."""
    try:
        return _output(_client(args).vm_exec(args.name, args.args, timeout_secs=args.timeout), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        if "timeout" in msg.lower():
            return _err(msg, exit_code=EXIT_TIMEOUT)
        return _err(msg)


def cmd_vm_resize(args) -> int:
    """Resize VM resources."""
    c = _client(args)
    if args.cpus is None and args.memory_bytes is None:
        return _err(
            "Nothing to resize",
            cause="Neither --cpus nor --memory-bytes was specified",
            fix="Provide at least one: --cpus N and/or --memory-bytes N",
            exit_code=EXIT_USAGE,
        )
    try:
        return _output(c.vm_resize(args.name, cpus=args.cpus, memory_bytes=args.memory_bytes), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def _simple_vm(method: str):
    """Factory for simple single-arg VM operations."""

    def f(args):
        try:
            return _output(getattr(_client(args), method)(args.name), args)
        except TerraError as e:
            msg = str(e)
            if "not found" in msg.lower():
                return _err(msg, exit_code=EXIT_NOTFOUND)
            return _err(msg)

    return f


def cmd_vm_attach(args) -> int:
    """Hot-plug layers to a running VM."""
    layers = _parse_multi(args.layers) if args.layers else []
    if not layers:
        return _err(
            "No layers specified",
            fix="Provide --layers L1,L2,...",
            exit_code=EXIT_USAGE,
        )
    try:
        return _output(_client(args).vm_attach_fs(args.name, layers), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_vm_detach(args) -> int:
    """Detach layers from a running VM."""
    try:
        return _output(_client(args).vm_detach_fs(args.name), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


# ═══════════════════════════════════════════════════════════════════
# pool commands
# ═══════════════════════════════════════════════════════════════════

def cmd_pool_ls(args) -> int:
    """List warm pools."""
    try:
        return _output(_client(args).pool_list(), args)
    except TerraError as e:
        return _err(str(e))


def cmd_pool_create(args) -> int:
    """Create a warm pool."""
    try:
        return _output(
            _client(args).pool_create(args.size, kernel=args.kernel, net=args.net),
            args,
        )
    except TerraError as e:
        return _err(str(e))


def cmd_pool_claim(args) -> int:
    """Claim an idle pool VM."""
    if args.template:
        t = Template.load(args.template)
        system_map = {"alpine": "base", "ubuntu": "ubuntu"}
        system = system_map.get(t.base, t.base)
        layers = t.layers + [system]
    elif args.layers:
        layers = _parse_multi(args.layers)
    else:
        return _err(
            "No layers or template specified",
            fix="Provide --template NAME or --layers L1,L2,...",
            exit_code=EXIT_USAGE,
        )
    try:
        return _output(_client(args).pool_claim(layers), args)
    except TerraError as e:
        return _err(str(e))


def cmd_pool_release(args) -> int:
    """Release a claimed pool VM back to idle."""
    try:
        return _output(_client(args).pool_release(args.name), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_pool_scale(args) -> int:
    """Scale a pool to a new size."""
    try:
        return _output(
            _client(args).pool_create(args.size, kernel=None, net=False),
            args,
        )
    except TerraError as e:
        return _err(str(e))


def cmd_pool_remove(args) -> int:
    """Remove (destroy) a pool VM."""
    try:
        return _output(_client(args).vm_destroy(args.name), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


# ═══════════════════════════════════════════════════════════════════
# net commands
# ═══════════════════════════════════════════════════════════════════

def cmd_net_ls(args) -> int:
    """List network interfaces."""
    try:
        return _output(_client(args)._send({"command": "net_list"}), args)
    except TerraError as e:
        return _err(str(e))


def cmd_net_create(args) -> int:
    """Bring up NAT networking."""
    try:
        return _output(_client(args)._send({"command": "net_up"}), args)
    except TerraError as e:
        return _err(str(e))


def cmd_net_remove(args) -> int:
    """Tear down NAT networking."""
    try:
        return _output(_client(args)._send({"command": "net_down"}), args)
    except TerraError as e:
        return _err(str(e))


# ═══════════════════════════════════════════════════════════════════
# daemon commands
# ═══════════════════════════════════════════════════════════════════

def cmd_daemon_start(args) -> int:
    """Start a daemon as a detached background process.

    Spawns a Python subprocess that calls Daemon.start(), which runs
    the engine in a Rust background thread via PyO3 FFI.
    """
    existing = _daemon_pids()
    if existing:
        print(f"daemon already running (pid={existing[0]}) — stop it first: terra daemon stop")
        return EXIT_ERROR

    log_file = paths.run_dir() / "daemon.log"

    cmd = [
        sys.executable,
        "-c",
        "from terra.daemon import Daemon; import time\n"
        "d = Daemon(tcp=%r).start()\n"
        "print(d.socket, flush=True)\n"
        "time.sleep(10**9)" % (args.tcp,),
    ]

    if args.daemonize or not args.daemonize:
        # Default: run as background daemon (existing behavior).
        # When --daemonize is explicit, same behavior but with log redirect.
        stdout_target = subprocess.DEVNULL
        stderr_target = subprocess.DEVNULL
        if args.daemonize:
            try:
                log_fh = open(str(log_file), "a")
                stdout_target = log_fh
                stderr_target = log_fh
            except OSError:
                pass

        proc = subprocess.Popen(
            cmd, stdout=stdout_target, stderr=stderr_target, start_new_session=True
        )
        (paths.run_dir() / "daemon.pid").write_text(str(proc.pid))
        time.sleep(1.5)
        sock = paths.default_socket()
        print(f"daemon started (pid={proc.pid}, socket={sock})")
        return EXIT_OK
    else:
        # Foreground mode (not daemonizing)
        print("Starting daemon in foreground...")
        proc = subprocess.Popen(cmd, stdout=None, stderr=None)
        try:
            proc.wait()
        except KeyboardInterrupt:
            proc.terminate()
            proc.wait()
        return EXIT_OK


def cmd_daemon_status(args) -> int:
    """Show daemon status (renamed from ls)."""
    info = {"socket": paths.default_socket()}
    pids = _daemon_pids()
    info["pids"] = pids
    info["alive"] = bool(pids) or Path(info["socket"]).exists()
    return _output(info, args)


def cmd_daemon_config(args) -> int:
    """Composed live view: engine, pool, network."""
    c = _client(args)
    out: dict = {}
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
    out["layers"] = (
        [e.name for e in sorted(paths.layers_dir().iterdir())]
        if paths.layers_dir().is_dir()
        else []
    )
    return _output(out, args)


def cmd_daemon_logs(args) -> int:
    """Show daemon logs, optionally following."""
    log_file = paths.run_dir() / "daemon.log"
    if not log_file.exists():
        print("(no daemon log file)")
        return EXIT_OK

    if args.follow:
        try:
            with open(str(log_file), "r") as f:
                # Show existing content
                print(f.read(), end="")
                # Follow new lines
                f.seek(0, os.SEEK_END)
                while True:
                    line = f.readline()
                    if line:
                        print(line, end="", flush=True)
                    else:
                        time.sleep(0.5)
        except KeyboardInterrupt:
            print()
            return EXIT_OK
    else:
        with open(str(log_file), "r") as f:
            print(f.read(), end="")
        return EXIT_OK


def cmd_daemon_stop(args) -> int:
    """Stop daemon gracefully."""
    return _daemon_stop(_signal.SIGTERM)


def cmd_daemon_destroy(args) -> int:
    """Force-stop daemon."""
    rc = _daemon_stop(_signal.SIGKILL)
    _daemon_pidfile().unlink(missing_ok=True)
    try:
        Path(paths.default_socket()).unlink()
    except FileNotFoundError:
        pass
    return rc


# ── daemon internals ──────────────────────────────────────────────

def _daemon_pidfile() -> Path:
    return paths.run_dir() / "daemon.pid"


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
    return EXIT_OK


# ═══════════════════════════════════════════════════════════════════
# main
# ═══════════════════════════════════════════════════════════════════

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
    terra pool create -n mypool --size 3
    terra pool claim --template python312

  Direct VM:
    terra vm create dev --kernel k612 --layers base --net
    terra vm exec dev -- python3 --version
    terra vm destroy dev
""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--socket", help="daemon socket path or tcp://host:port")
    p.add_argument("--json", action="store_true", help="machine-readable JSON output")
    p.add_argument("-v", "--verbose", action="store_true", help="verbose output (--json style for dicts)")
    sub = p.add_subparsers(dest="cmd", required=True)

    # ── sandbox ────────────────────────────────────────────────────
    sb = sub.add_parser("sandbox", help="high-level sandbox operations (recommended)")
    sbs = sb.add_subparsers(dest="action", required=True)

    sp = sbs.add_parser("create", help="create a sandbox from a template")
    sp.add_argument("--template", help="template name")
    sp.add_argument("-n", "--name", help="sandbox name (auto-generated if omitted)")
    sp.add_argument("--layers", nargs="*", help="explicit layer names (comma-separated)")
    sp.add_argument("--kernel", help="kernel variant name or path")
    sp.add_argument("--cpu", type=int, default=1, help="vCPU count (default: 1)")
    sp.add_argument("--memory", type=int, default=256, metavar="MB", help="memory in MiB (default: 256)")
    sp.add_argument("--disk", type=int, metavar="MB", help="disk size in MiB (reserved)")
    sp.add_argument("--net", action="store_true", help="attach NAT networking")
    sp.add_argument("--env", nargs="*", help="environment variables (KEY=VALUE)")
    sp.add_argument("--timeout", type=int, default=600, help="default command timeout (seconds)")
    sp.add_argument("--backend", default="auto", choices=["auto", "ch", "sandlock"])
    sp.set_defaults(f=cmd_sandbox_create)

    sbs.add_parser("ls", help="list running sandboxes").set_defaults(f=cmd_sandbox_ls)

    sp = sbs.add_parser("info", help="show sandbox details")
    sp.add_argument("id", help="sandbox ID")
    sp.set_defaults(f=cmd_sandbox_info)

    sp = sbs.add_parser("exec", help="execute command in sandbox")
    sp.add_argument("id", help="sandbox ID")
    sp.add_argument("--cwd", default="/workdir", help="working directory")
    sp.add_argument("--env", nargs="*", help="environment variables (KEY=VALUE)")
    sp.add_argument("--timeout", type=int, default=60, help="command timeout (seconds)")
    sp.add_argument("--detach", action="store_true", help="detached mode (reserved)")
    sp.add_argument("--follow", help="follow exec ID (reserved)")
    sp.add_argument("args", nargs=argparse.REMAINDER, help="command and arguments (after --)")
    sp.set_defaults(f=cmd_sandbox_exec)

    sp = sbs.add_parser("cp", help="copy files between host and sandbox")
    sp.add_argument("src", help="local path or REMOTE:/path")
    sp.add_argument("dst", help="local path or REMOTE:/path")
    sp.set_defaults(f=cmd_sandbox_cp)

    sp = sbs.add_parser("resize", help="resize sandbox resources")
    sp.add_argument("id", help="sandbox ID")
    sp.add_argument("--cpu", type=int, help="new vCPU count")
    sp.add_argument("--memory", type=int, metavar="MB", help="new memory in MiB")
    sp.set_defaults(f=cmd_sandbox_resize)

    sp = sbs.add_parser("metrics", help="query sandbox metrics")
    sp.add_argument("id", help="sandbox ID")
    sp.set_defaults(f=cmd_sandbox_metrics)

    sp = sbs.add_parser("kill", help="force-kill a sandbox")
    sp.add_argument("id", help="sandbox ID")
    sp.set_defaults(f=cmd_sandbox_kill)

    # ── template ───────────────────────────────────────────────────
    tmpl = sub.add_parser("template", help="manage named templates")
    tmpls = tmpl.add_subparsers(dest="action", required=True)

    tmpls.add_parser("ls", help="list templates").set_defaults(f=cmd_template_ls)

    sp = tmpls.add_parser("create", help="create a template")
    sp.add_argument("-n", "--name", required=True, help="template name")
    sp.add_argument("--kernel", required=True, help="kernel variant")
    sp.add_argument("--rootfs", required=True, choices=["alpine", "ubuntu"], help="base distro")
    sp.add_argument("--layers", nargs="*", default=[], help="tool layers comma-separated")
    sp.set_defaults(f=cmd_template_create)

    sp = tmpls.add_parser("info", help="show template details")
    sp.add_argument("name", help="template name")
    sp.set_defaults(f=cmd_template_info)

    sp = tmpls.add_parser("remove", help="remove a template")
    sp.add_argument("name", help="template name")
    sp.set_defaults(f=cmd_template_remove)

    # ── image ───────────────────────────────────────────────────────
    img = sub.add_parser("image", help="manage guest images (kernel, rootfs, initramfs)")
    imgs = img.add_subparsers(dest="action", required=True)

    imgs.add_parser("ls", help="list images").set_defaults(f=cmd_image_ls)

    b = imgs.add_parser("build", help="build images")
    bs = b.add_subparsers(dest="what", required=True)

    k = bs.add_parser("kernel", help="build a kernel image")
    k.add_argument("-n", "--name", required=True, help="kernel variant name")
    k.add_argument("--version", default="6.12", help="kernel version")
    k.set_defaults(f=cmd_image_build_kernel)

    r = bs.add_parser("rootfs", help="build a rootfs image")
    r.add_argument("-n", "--name", required=True, choices=["alpine", "ubuntu"])
    r.set_defaults(f=cmd_image_build_rootfs)

    i = bs.add_parser("initramfs", help="build an initramfs image")
    i.add_argument("--type", required=True, choices=["virtiofs", "agent"])
    i.add_argument("-n", "--name", help="variant name (optional)")
    i.set_defaults(f=cmd_image_build_initramfs)

    sp = imgs.add_parser("info", help="show image details")
    sp.add_argument("name", help="image name")
    sp.set_defaults(f=cmd_image_info)

    sp = imgs.add_parser("remove", help="remove an image")
    sp.add_argument("-n", "--name", required=True, help="image name")
    sp.set_defaults(f=cmd_image_remove)

    # ── layer ───────────────────────────────────────────────────────
    lay = sub.add_parser("layer", help="manage filesystem layers")
    lays = lay.add_subparsers(dest="action", required=True)

    lays.add_parser("ls", help="list layers").set_defaults(f=cmd_layer_ls)

    sp = lays.add_parser("create", help="create a layer")
    sp.add_argument("-n", "--name", required=True, help="layer name")
    src = sp.add_mutually_exclusive_group(required=True)
    src.add_argument("--from-dir", help="pack an existing directory")
    src.add_argument("--script", help="build-by-doing: run setup in a builder VM")
    src.add_argument("--from-image", action="store_true", help="base layer from guest rootfs")
    sp.add_argument("--rootfs", required=True,
                    help="system the layer is built on: alpine (musl) or ubuntu (glibc)")
    sp.add_argument("--kernel", help="kernel for builder VM (required with --script)")
    sp.add_argument("--no-net", action="store_true", help="disable networking for builder VM")
    sp.add_argument("--timeout", type=int, default=600, help="builder VM timeout")
    sp.set_defaults(f=cmd_layer_create)

    sp = lays.add_parser("info", help="show layer details")
    sp.add_argument("name", help="layer name")
    sp.set_defaults(f=cmd_layer_info)

    sp = lays.add_parser("remove", help="remove a layer")
    sp.add_argument("-n", "--name", required=True, help="layer name")
    sp.set_defaults(f=cmd_layer_remove)

    # ── vm ──────────────────────────────────────────────────────────
    vm = sub.add_parser("vm", help="VM operations")
    vms = vm.add_subparsers(dest="action", required=True)

    vms.add_parser("ls", help="list VMs").set_defaults(f=cmd_vm_ls)

    sp = vms.add_parser("create", help="create a VM")
    sp.add_argument("name", help="VM name")
    sp.add_argument("--kernel", required=True, help="kernel variant or path")
    sp.add_argument("--rootfs", "--initramfs", dest="rootfs", help="rootfs/initramfs name")
    sp.add_argument("--cpus", type=int, default=2, help="vCPUs (default: 2)")
    sp.add_argument("--max-cpus", type=int, help="max vCPUs")
    sp.add_argument("--memory", type=int, default=512, metavar="MB", help="memory MiB (default: 512)")
    sp.add_argument("--max-memory", type=int, metavar="MB", help="max memory MiB")
    sp.add_argument("--layers", nargs="*", default=[], help="layer names (comma-separated)")
    sp.add_argument("--system", help="system base layer (default: base)")
    sp.add_argument("--upper", help="persistent upperdir name")
    sp.add_argument("--net", action="store_true", help="attach NAT networking")
    sp.set_defaults(f=cmd_vm_create)

    sp = vms.add_parser("info", help="show VM details")
    sp.add_argument("name", help="VM name")
    sp.set_defaults(f=cmd_vm_info)

    sp = vms.add_parser("exec", help="execute command in VM")
    sp.add_argument("name", help="VM name")
    sp.add_argument("--timeout", type=int, default=60, help="command timeout (seconds)")
    sp.add_argument("args", nargs=argparse.REMAINDER, help="command and arguments (after --)")
    sp.set_defaults(f=cmd_vm_exec)

    sp = vms.add_parser("resize", help="resize VM resources")
    sp.add_argument("name", help="VM name")
    sp.add_argument("--cpus", type=int, help="new vCPU count")
    sp.add_argument("--memory-bytes", type=int, help="new memory in bytes")
    sp.set_defaults(f=cmd_vm_resize)

    sp = vms.add_parser("attach", help="hot-plug layers to a running VM")
    sp.add_argument("name", help="VM name")
    sp.add_argument("--layers", nargs="+", required=True, help="layer names (comma-separated)")
    sp.set_defaults(f=cmd_vm_attach)

    sp = vms.add_parser("detach", help="hot-unplug layers from a running VM")
    sp.add_argument("name", help="VM name")
    sp.set_defaults(f=cmd_vm_detach)

    # Backward-compat hidden aliases for attach-fs / detach-fs
    sp = vms.add_parser("attach-fs", help=argparse.SUPPRESS)
    sp.add_argument("name")
    sp.add_argument("--layers", nargs="+", required=True)
    sp.set_defaults(f=cmd_vm_attach)

    sp = vms.add_parser("detach-fs", help=argparse.SUPPRESS)
    sp.add_argument("name")
    sp.set_defaults(f=cmd_vm_detach)

    for act, method in (
        ("shutdown", "vm_shutdown"),
        ("kill", "vm_kill"),
        ("destroy", "vm_destroy"),
    ):
        sp = vms.add_parser(act, help=f"{act} a VM")
        sp.add_argument("name", help="VM name")
        sp.set_defaults(f=_simple_vm(method))

    # ── pool ────────────────────────────────────────────────────────
    pool = sub.add_parser("pool", help="warm pool operations")
    pools = pool.add_subparsers(dest="action", required=True)

    pools.add_parser("ls", help="list pools").set_defaults(f=cmd_pool_ls)

    sp = pools.add_parser("create", help="create a warm pool")
    sp.add_argument("-n", "--name", help="pool name (reserved)")
    sp.add_argument("--size", type=int, default=1, help="number of idle VMs")
    sp.add_argument("--kernel", help="kernel variant")
    sp.add_argument("--net", action="store_true", help="enable NAT networking")
    sp.set_defaults(f=cmd_pool_create)

    sp = pools.add_parser("claim", help="claim an idle pool VM")
    sp.add_argument("name", nargs="?", help="pool name (reserved)")
    sp.add_argument("--template", help="template name for layers")
    sp.add_argument("--layers", nargs="+", help="explicit layers (comma-separated)")
    sp.set_defaults(f=cmd_pool_claim)

    sp = pools.add_parser("release", help="release a claimed VM back to idle")
    sp.add_argument("name", help="VM name")
    sp.set_defaults(f=cmd_pool_release)

    sp = pools.add_parser("scale", help="scale pool to new size")
    sp.add_argument("name", nargs="?", help="pool name (reserved)")
    sp.add_argument("--size", type=int, required=True, help="new pool size")
    sp.set_defaults(f=cmd_pool_scale)

    sp = pools.add_parser("remove", help="remove a pool VM")
    sp.add_argument("name", help="VM name")
    sp.set_defaults(f=cmd_pool_remove)

    # ── net ─────────────────────────────────────────────────────────
    net = sub.add_parser("net", help="NAT networking")
    nets = net.add_subparsers(dest="action", required=True)

    nets.add_parser("ls", help="list networks").set_defaults(f=cmd_net_ls)

    sp = nets.add_parser("create", help="create NAT network")
    sp.add_argument("-n", "--name", nargs="?", const="default", default="default",
                    help="network name (default: 'default')")
    sp.set_defaults(f=cmd_net_create)

    sp = nets.add_parser("remove", help="remove NAT network")
    sp.add_argument("name", help="network name")
    sp.set_defaults(f=cmd_net_remove)

    # ── daemon ──────────────────────────────────────────────────────
    dmn = sub.add_parser("daemon", help="engine daemon lifecycle")
    dmns = dmn.add_subparsers(dest="action", required=True)

    sp = dmns.add_parser("start", help="start the daemon")
    sp.add_argument("--daemonize", action="store_true", help="run as background daemon")
    sp.add_argument("--tcp", help="also listen on host:port for remote clients")
    sp.set_defaults(f=cmd_daemon_start)

    dmns.add_parser("status", help="show daemon status").set_defaults(f=cmd_daemon_status)

    dmns.add_parser("config", help="show live daemon state").set_defaults(f=cmd_daemon_config)

    sp = dmns.add_parser("logs", help="show daemon logs")
    sp.add_argument("-f", "--follow", action="store_true", help="follow log output")
    sp.set_defaults(f=cmd_daemon_logs)

    dmns.add_parser("stop", help="stop daemon gracefully").set_defaults(f=cmd_daemon_stop)

    dmns.add_parser("destroy", help="force-stop daemon").set_defaults(f=cmd_daemon_destroy)

    # Parse and dispatch
    args = p.parse_args()

    try:
        return args.f(args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        if "timeout" in msg.lower():
            return _err(msg, exit_code=EXIT_TIMEOUT)
        return _err(msg)
    except (FileNotFoundError, ConnectionRefusedError) as e:
        return _err(
            "Cannot connect to engine daemon",
            cause=f"No daemon running at {paths.default_socket()} ({e})",
            fix="Run 'terra daemon start' to start the engine",
            exit_code=EXIT_DAEMON,
        )
    except PermissionError:
        return _err(
            "Socket permission denied",
            cause=f"Socket at {paths.default_socket()} is owned by another user (root?)",
            fix="Start your own daemon: terra daemon start",
            exit_code=EXIT_DAEMON,
        )
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())

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

from . import assets, images, paths
from .client import TerraClient, TerraError
from .pool import scale_pool
from .sandbox import Sandbox, _SYSTEM_MAP
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


def _output_exec(resp, args) -> int:
    """Print an exec response and propagate the guest exit code."""
    rc = _output(resp, args)
    if rc != EXIT_OK:
        return rc
    code = resp.get("exit_code") if isinstance(resp, dict) else None
    if not isinstance(code, int):
        return EXIT_OK
    return code if 0 <= code <= 255 else 1


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


def _repo_root() -> Path | None:
    """Terrarium repo checkout root, or None for wheel-only installs.

    The package lives at <repo>/sdk/python/terra/__main__.py, so the
    repo root is parents[3]. Verified by the presence of images/build.sh.
    """
    root = Path(__file__).resolve().parents[3]
    return root if (root / "images" / "build.sh").exists() else None


def _fs_root() -> Path:
    """Engine fs root — must match the daemon's TERRA_STATE_DIR."""
    return Path(os.environ.get("TERRA_STATE_DIR") or str(paths.state_dir())) / "fs"


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

def _build_policy_from_args(args) -> dict | None:
    """Assemble a SandboxPolicy dict from --net-allow/--read-path/etc flags.

    CLI flags map onto the new policy JSON shape (D5)::

        --read-path /opt    → {"File": {"path": {"Prefix": "/opt"}, "access": "Read"}}
        --write-path /out   → {"File": {"path": {"Prefix": "/out"}, "access": "ReadWrite"}}
        --net-allow h:p     → {"Network": {"endpoint": {"host": "h", "port": p},
                                           "direction": "Outbound"}}
        --memory-mb N       → limits.memory_mb
        --procs N           → limits.procs

    ``--net-allow`` accepts ``host`` or ``host:port`` (port is None
    when omitted). Returns None when no policy flags were given.
    """
    capabilities: list[dict] = []
    for p in getattr(args, "read_path", None) or []:
        capabilities.append({"File": {"path": {"Prefix": p}, "access": "Read"}})
    for p in getattr(args, "write_path", None) or []:
        capabilities.append(
            {"File": {"path": {"Prefix": p}, "access": "ReadWrite"}}
        )
    for na in getattr(args, "net_allow", None) or []:
        host, _, port = na.partition(":")
        capabilities.append(
            {
                "Network": {
                    "endpoint": {
                        "host": host,
                        "port": int(port) if port else None,
                    },
                    "direction": "Outbound",
                }
            }
        )

    limits: dict = {}
    if getattr(args, "memory_mb", None) is not None:
        limits["memory_mb"] = args.memory_mb
    if getattr(args, "procs", None) is not None:
        limits["procs"] = args.procs

    policy: dict = {}
    if capabilities:
        policy["capabilities"] = capabilities
    if limits:
        policy["limits"] = limits
    return policy or None


def cmd_sandbox_create(args) -> int:
    """Create a sandbox using the high-level SDK."""
    try:
        policy = _build_policy_from_args(args)
        sb = Sandbox(
            template=args.template,
            layers=args.layers or None,
            kernel=args.kernel,
            cpu=args.cpu,
            memory_mb=args.memory,
            network=bool(args.net),
            policy=policy,
            env=_parse_kv_pairs(args.env),
            timeout=args.timeout,
            pool=not args.no_pool,
        )
        out = {
            "id": sb.id,
            "name": sb.id,
            "vm": sb.vm,
            "status": sb.status,
            "backend": sb.backend,
            "pool_backed": sb.pool_backed,
        }
        # The CLI returns the handle to the user — don't let Sandbox.__del__
        # kill the freshly created record when this process exits.
        sb._alive = False
        return _output(out, args)
    except FileNotFoundError as e:
        return _err(str(e), exit_code=EXIT_NOTFOUND)
    except Exception as e:
        return _err(str(e))


def cmd_sandbox_ls(args) -> int:
    """List sandboxes (engine registry)."""
    c = _client(args)
    try:
        return _output(c.sandbox_list(), args)
    except TerraError as e:
        return _err(str(e))


def _sandbox_vm(c: TerraClient, id_or_vm: str) -> str:
    """Resolve a sandbox id to its tenant VM name (falls back to the
    given value so plain VM names keep working)."""
    try:
        return c.sandbox_info(id_or_vm)["vm"]
    except TerraError:
        return id_or_vm


def cmd_sandbox_info(args) -> int:
    """Show details about a specific sandbox."""
    c = _client(args)
    try:
        return _output(c.sandbox_info(args.id), args)
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

        # Sandboxed by default (engine default too); --no-sandbox opts out.
        policy = _build_policy_from_args(args)
        if args.detach:
            # Background exec: return immediately with a session_id (poll via `sandbox session status|kill`).
            resp = c.sandbox_exec(args.id, cmd_args, args.timeout,
                                  sandbox=not args.no_sandbox,
                                  exec_mode="background",
                                  policy=policy)
            return _output(
                {
                    "session_id": resp.get("session_id"),
                    "sandbox": resp.get("sandbox", args.id),
                    "status": resp.get("status"),
                },
                args,
            )
        resp = c.sandbox_exec(args.id, cmd_args, args.timeout,
                              sandbox=not args.no_sandbox,
                              policy=policy)
        return _output_exec(resp, args)
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
            resp = c.vm_exec(_sandbox_vm(c, sandbox_id), ["cat", remote_path], timeout_secs=30)
            content = resp.get("stdout", "")
            with open(dst, "w") as f:
                f.write(content)
            print(f"Downloaded {src} → {dst}")
        else:
            # Upload: local → sandbox:/path
            sandbox_id, remote_path = dst.split(":", 1)
            with open(src, "rb") as f:
                data = base64.b64encode(f.read()).decode()
            c.vm_exec(_sandbox_vm(c, sandbox_id), ["sh", "-c", f"echo {data} | base64 -d > {remote_path}"], timeout_secs=30)
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
        resp = c.vm_resize(_sandbox_vm(c, args.id), **kwargs)
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
        info = c.vm_info(_sandbox_vm(c, args.id))
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
    """Kill a sandbox (sessions + workdir; the tenant VM keeps running)."""
    c = _client(args)
    try:
        resp = c.sandbox_kill(args.id)
        return _output(resp, args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_sandbox_destroy_tenant(args) -> int:
    """Destroy a tenant VM and all its sandboxes."""
    c = _client(args)
    try:
        return _output(c.tenant_destroy(args.tenant), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


# ── background exec sessions (from `sandbox exec --detach`) ─────

def cmd_session_status(args) -> int:
    """Show a background exec session's status."""
    c = _client(args)
    try:
        return _output(c.session_status(args.session_id), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_session_kill(args) -> int:
    """Kill a background exec session."""
    c = _client(args)
    try:
        return _output(c.session_kill(args.session_id), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_session_ls(args) -> int:
    """List background exec sessions."""
    c = _client(args)
    try:
        return _output(c.session_list(), args)
    except TerraError as e:
        return _err(str(e))


# ═══════════════════════════════════════════════════════════════════
# setup — one-command environment bootstrap
# ═══════════════════════════════════════════════════════════════════

def cmd_setup(args) -> int:
    """One-command setup: assets + kernel + rootfs + initramfs + base layer + template.

    Every stage is idempotent — existing artifacts are skipped unless
    --force is given.
    """
    distro = args.distro
    force = args.force
    system_layer = _SYSTEM_MAP[distro]

    def stage(msg: str) -> None:
        print(f"\n==> {msg}")

    # 1. host binaries (auto-download into the managed bin dir)
    stage("host binaries (cloud-hypervisor, virtiofsd, erofs tools)")
    try:
        print(f"  cloud-hypervisor: {assets.ensure_ch()}")
        print(f"  virtiofsd:        {assets.ensure_virtiofsd()}")
        mkfs, fuse = assets.ensure_erofs_tools()
        print(f"  erofs tools:      {mkfs}, {fuse}")
    except assets.AssetError as e:
        return _err(str(e))

    # 2. default kernel (Sandbox(kernel=None) and build_daemon_env use it)
    stage("default kernel")
    rc = _build_kernel_image("default", args.kernel_version, force=force)
    if rc != EXIT_OK:
        return rc

    # 3. distro rootfs image
    stage(f"{distro} rootfs")
    try:
        img = _ensure_distro_rootfs(distro, force=force)
        print(f"  rootfs: {img}")
    except Exception as e:
        return _err(str(e))

    # 4. initramfs images (agent for warm/exec, virtiofs for layered boot)
    stage("initramfs (agent + virtiofs)")
    for name in ("initramfs-agent.cpio.gz", "initramfs-virtiofs.cpio.gz"):
        managed = paths.rootfs_dir() / name
        if managed.exists() and not force:
            print(f"  already present: {managed}")
            continue
        if force:
            managed.unlink(missing_ok=True)
        try:
            print(f"  built: {images.ensure(name)}")
        except Exception as e:
            return _err(str(e))

    # 5. base layer (alpine→base, ubuntu→ubuntu — sandbox/template rely on it)
    stage(f"base layer ({system_layer})")
    layer_dir = paths.layers_dir() / system_layer
    if distro == "ubuntu":
        # The distro script created layers/ubuntu during the rootfs stage.
        if not layer_dir.is_dir():
            return _err(f"ubuntu layer missing after rootfs stage: {layer_dir}")
        print(f"  ready: {layer_dir}")
    else:
        try:
            dest = _extract_rootfs_into_layer(img, system_layer, force=force)
        except Exception as e:
            return _err(str(e))
        print(f"  {'extracted' if dest else 'already present'}: {layer_dir}")

    # 6. guest binaries — bake sandlock (exec isolation) and the current
    # guest-proxy agent into the system layer: cold-boot VMs switch_root
    # into the composed layers and run the agent from there, so a layer
    # built from an older rootfs serves a stale agent otherwise.
    stage("guest binaries (sandlock + guest-proxy)")
    rc = _install_sandlock(system_layer, force=force)
    if rc != EXIT_OK:
        return rc
    rc = _refresh_layer_guest_proxy(system_layer, force=force)
    if rc != EXIT_OK:
        return rc

    # 7. template (same code path as `template create`)
    stage(f"template ({distro})")
    t = Template(name=distro, base=distro, layers=[system_layer], kernel="default")
    path = t.save()
    print(f"  wrote: {path}")

    print(f"""
setup complete. Next:

  terra daemon start
  terra sandbox create --template {distro} --net
  terra sandbox exec <vm-name> -- python3 --version
  terra sandbox kill <vm-name>""")
    return EXIT_OK


# ═══════════════════════════════════════════════════════════════════
# setup/build internals (shared by `terra setup` and `tool create`;
# the image/template/layer CLI groups were removed — setup is the
# only system-resource entry point)
# ═══════════════════════════════════════════════════════════════════

def _build_kernel_image(name: str, version: str, *, force: bool = False) -> int:
    """Build kernel variant *name* (idempotent skip unless *force*)."""
    dest = paths.kernels_dir() / name / "vmlinux.bin"
    if dest.exists() and not force:
        print(f"already present: {dest}")
        return EXIT_OK
    repo = _repo_root()
    if repo is None:
        return _err(
            "Terrarium repo checkout not found",
            cause="Kernel builds need images/build-kernel.sh from the repo",
            fix="Run on a machine with the Terrarium repo checked out",
        )
    script = repo / "images" / "build-kernel.sh"
    with tempfile.TemporaryDirectory() as td:
        r = subprocess.run(
            ["bash", str(script), version, "", td]
        )
        if r.returncode:
            return r.returncode
        src = Path(td) / "vmlinux.bin"
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(src), str(dest))
        print(f"built: {dest}")
        return EXIT_OK


# ── internal build helpers ───────────────────────────────────────


def _refresh_layer_guest_proxy(system_layer: str, *, force: bool = False) -> int:
    """Sync <layer>/bin/guest-proxy with the current musl build.

    Cold-boot layered VMs switch_root into the composed layers, so the
    exec agent they run is the layer's copy — rebuilds of crates/guest-proxy
    must propagate here. Idempotent (sha256); --force reinstalls.
    """
    repo = _repo_root()
    if repo is None:
        print("  (skip guest-proxy refresh — no repo checkout)")
        return EXIT_OK
    gp = repo / "target" / "x86_64-unknown-linux-musl" / "release" / "guest-proxy"
    if not gp.exists():
        subprocess.run(
            ["cargo", "build", "--release",
             "--target", "x86_64-unknown-linux-musl", "-p", "guest-proxy"],
            cwd=str(repo), check=True,
        )
    dest = paths.layers_dir() / system_layer / "bin" / "guest-proxy"
    if dest.exists() and not force and _sha256_file(dest) == _sha256_file(gp):
        print(f"  guest-proxy current: {dest}")
        return EXIT_OK
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy(gp, dest)
    dest.chmod(0o755)
    print(f"  guest-proxy updated: {dest}")
    return EXIT_OK


def _sha256_file(p: Path) -> str:
    import hashlib

    return hashlib.sha256(p.read_bytes()).hexdigest()


def _install_sandlock(system_layer: str, *, force: bool = False) -> int:
    """Install the musl sandlock binary into the system layer (idempotent).

    Builds bin/sandlock-musl via images/build-sandlock.sh when absent,
    then installs it as <layer>/usr/bin/sandlock (0755). Skipped when
    the installed copy is identical (sha256); --force reinstalls.
    """
    src = paths.bin_dir() / "sandlock-musl"
    if not src.exists():
        repo = _repo_root()
        script = repo / "images" / "build-sandlock.sh" if repo else None
        if script is None or not script.exists():
            return _err(
                "sandlock-musl not found and no build script available",
                cause=f"missing: {src} (and images/build-sandlock.sh not found)",
                fix="Run on a machine with the Terrarium repo checked out",
            )
        r = subprocess.run(["bash", str(script)])
        if r.returncode:
            return r.returncode
    dest = paths.layers_dir() / system_layer / "usr" / "bin" / "sandlock"
    if dest.exists() and not force and _sha256_file(dest) == _sha256_file(src):
        print(f"  already present: {dest}")
        return EXIT_OK
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy(src, dest)
    dest.chmod(0o755)
    print(f"  installed: {dest}")
    return EXIT_OK


def _ensure_distro_rootfs(distro: str, *, force: bool = False) -> Path:
    """Ensure images/rootfs/<distro>.cpio(.gz), building if needed.

    alpine: busybox rootfs dir via images/build-rootfs.sh, packed with
    terrarium_fs (inside images.ensure). ubuntu: images/build-layer-distro.sh
    creates the layer dir, then it is packed into the rootfs image.
    Raises on failure — callers convert to _err.
    """
    import terrarium_fs

    rdir = paths.rootfs_dir()
    if distro not in _SYSTEM_MAP:
        raise ValueError(f"unsupported distro {distro!r}")
    if not force:
        for cand in (rdir / f"{distro}.cpio", rdir / f"{distro}.cpio.gz"):
            if cand.exists():
                return cand
    if distro == "alpine":
        if force:
            repo = _repo_root()
            if repo:
                (repo / "target" / "guest" / "alpine.cpio").unlink(missing_ok=True)
            (rdir / "alpine.cpio").unlink(missing_ok=True)
        return images.ensure("alpine.cpio")
    # ubuntu
    layer_dir = Path(os.environ.get("TERRA_LAYER_DIR") or paths.layers_dir()) / "ubuntu"
    if force:
        (layer_dir / ".unpacked").unlink(missing_ok=True)
    if not layer_dir.is_dir() or not (layer_dir / ".unpacked").exists():
        repo = _repo_root()
        if repo is None:
            raise FileNotFoundError(
                "ubuntu rootfs build needs images/build-layer-distro.sh "
                "from a Terrarium repo checkout"
            )
        subprocess.run(
            ["bash", str(repo / "images" / "build-layer-distro.sh"), "ubuntu"],
            check=True,
        )
    out = terrarium_fs.pack_cpio_rootfs(str(layer_dir), "ubuntu", str(rdir))
    return Path(out)


# ═══════════════════════════════════════════════════════════════════
# tool commands — tool layers built on distro templates
# ═══════════════════════════════════════════════════════════════════

def cmd_tool_ls(args) -> int:
    """List filesystem layers."""
    import terrarium_fs

    layer_dir = os.environ.get("TERRA_LAYER_DIR") or str(paths.layers_dir())
    try:
        names = terrarium_fs.list_layers(layer_dir)
        return _output(list(names), args)
    except Exception as e:
        return _err(f"read {layer_dir}: {e}")


def cmd_tool_create(args) -> int:
    """Build a tool layer by provisioning a builder VM from a template."""
    try:
        t = Template.load(args.template)
    except FileNotFoundError:
        return _err(
            f"Template not found: {args.template!r}",
            cause="Tool layers are built on a distro template",
            fix=f"Create the distro environment first: terra setup {args.template}",
            exit_code=EXIT_NOTFOUND,
        )
    system = _SYSTEM_MAP.get(t.base)
    if system is None:
        return _err(
            f"Unsupported template base {t.base!r}",
            cause="Only 'alpine' (musl) and 'ubuntu' (glibc) are supported",
            fix="Use a template created by `terra setup alpine|ubuntu`",
            exit_code=EXIT_USAGE,
        )
    return _build_tool_layer(args, system, t.kernel or "default")


def _extract_rootfs_into_layer(rootfs_img: Path, layer_name: str, *, force: bool = False) -> Path | None:
    """Extract a rootfs cpio into layers/<layer_name> (idempotent).

    Returns the layer path, or None when it already exists and not *force*.
    Used by `terra setup` to materialize the distro base layer.
    """
    import terrarium_fs

    dest = paths.layers_dir() / layer_name
    if dest.exists():
        if not force:
            return None
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    terrarium_fs.extract_cpio_layer(str(rootfs_img), str(dest))
    return dest


def _build_tool_layer(args, system: str, kernel: str) -> int:
    """Build a tool layer by configuring inside a builder VM."""
    client = _client(args)
    name = args.name
    builder = f"lb-{name}"

    # 1) builder VM from the base layer, persistent upper
    try:
        client.vm_create(
            builder,
            kernel if Path(kernel).exists() else str(images.resolve_kernel(kernel)),
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

    # Wait for VM to be fully ready before exec
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            info = client.vm_info(builder)
            if info.get("state") == "Running":
                break
        except TerraError:
            pass
        time.sleep(0.5)
    else:
        client.vm_destroy(builder)
        return _err(f"builder VM {builder} did not reach Running state within 30s")

    # 2) provision inside (if a script was given)
    if args.script:
        try:
            content = Path(args.script).read_text()
        except OSError as e:
            try:
                client.vm_destroy(builder)
            except TerraError:
                pass
            return _err(f"read script: {e}")
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
        # apt leaves root-owned 0700 dirs (/var/cache/apt/archives/partial)
        # that break the read-only erofs pack of the upperdir — purge
        # caches so the delta is packable by the invoking (non-root) user.
        ["sh", "-c", "rm -rf /tmp/* /run/* /var/log/* /var/cache/apt/* /var/lib/apt/lists/* /etc/resolv.conf 2>/dev/null; sync"],
        timeout_secs=30,
    )

    # 4) destroy builder
    try:
        client.vm_destroy(builder)
    except TerraError:
        pass
    print("builder VM destroyed")

    # 5) pack the upperdir delta as the layer
    upper_dir = _fs_root() / "uppers" / builder
    if not upper_dir.is_dir():
        return _err(
            f"upperdir {upper_dir} not found — tool build needs a LOCAL daemon "
            "(the upperdir lives on the daemon host)"
        )
    out = images.build_layer(str(upper_dir), name)
    print(f"tool layer '{name}' built: {out}")
    return EXIT_OK


def cmd_tool_remove(args) -> int:
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
        return _output_exec(_client(args).vm_exec(args.name, args.args, timeout_secs=args.timeout), args)
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


def cmd_vm_snapshot(args) -> int:
    """Capture a VM state for fast reset (P1)."""
    c = _client(args)
    try:
        return _output(c.vm_snapshot(args.name, args.path), args)
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_vm_restore(args) -> int:
    """Create a NEW VM from a snapshot (P1 fast reset)."""
    c = _client(args)
    try:
        return _output(
            c.vm_restore(args.name, args.snapshot, layers=args.layers, net=args.net),
            args,
        )
    except TerraError as e:
        msg = str(e)
        if "not found" in msg.lower():
            return _err(msg, exit_code=EXIT_NOTFOUND)
        return _err(msg)


def cmd_audit_ls(args) -> int:
    """Query the engine's audit ring buffer."""
    c = _client(args)
    try:
        resp = c.audit_list(limit=args.limit, event=args.event, sandbox_id=args.id)
    except TerraError as e:
        return _err(str(e))
    records = resp.get("audit", [])
    if args.raw:
        return _output(resp, args)
    for r in records:
        line = f"{r.get('event')} {r.get('sandbox_id')}"
        if r.get("args"):
            line += f" {' '.join(r['args'])}"
        if r.get("exit_code") is not None:
            line += f" exit={r['exit_code']}"
        if r.get("reason"):
            line += f" reason={r['reason']}"
        if r.get("kind"):
            line += f" kind={r['kind']}"
        print(line)
    return EXIT_OK


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
        system = _SYSTEM_MAP.get(t.base, t.base)
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
            scale_pool(
                _client(args),
                args.size,
                kernel=args.kernel,
                net=args.net,
            ),
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

    Spawns a Python subprocess that calls Daemon.start(embedded=False),
    which runs the engine in a Rust background thread via PyO3 FFI.
    embedded=False makes this a dedicated service daemon that honors
    the daemon_stop wire command.

    NAT networking (tap) needs CAP_NET_ADMIN, so a non-root invocation
    self-elevates: it re-execs itself via sudo (preserving PATH, HOME
    and TERRA_* vars). --no-root skips the elevation for rootless use.
    """
    if os.geteuid() != 0 and not args.no_root:
        sudo = shutil.which("sudo")
        sudo_noninteractive = False
        if sudo:
            r = subprocess.run([sudo, "-n", "true"], capture_output=True)
            sudo_noninteractive = r.returncode == 0
        if sudo and (sudo_noninteractive or sys.stdin.isatty()):
            print("daemon runs as root (NAT networking) — re-running via sudo ...")
            env_pairs = [
                f"PATH={os.environ.get('PATH', '')}",
                f"HOME={os.environ.get('HOME', '')}",
            ]
            env_pairs += [
                f"{k}={v}" for k, v in os.environ.items() if k.startswith("TERRA_")
            ]
            cmd = [
                sudo, "env", *env_pairs,
                sys.executable, "-m", "terra", "daemon", "start", "--no-root",
            ]
            if args.tcp:
                cmd += ["--tcp", args.tcp]
            os.execvp(sudo, cmd)
        # sudo unavailable or needs a password we can't supply here —
        # don't hang on a hidden prompt; hand the user the command.
        manual = (
            f'sudo env "PATH={os.environ.get("PATH", "")}" '
            f'"HOME={os.environ.get("HOME", "")}" '
            f"{sys.executable} -m terra daemon start"
        )
        return _err(
            "daemon start needs root for NAT networking (tap device)",
            cause="not running as root and sudo is unavailable or needs a password",
            fix=f"run it yourself: {manual}\n"
            "  or go rootless (no --net VMs): terra daemon start --no-root",
        )

    if _daemon_alive(paths.default_socket()):
        print("daemon already running — stop it first: terra daemon stop")
        return EXIT_ERROR

    log_file = paths.run_dir() / "daemon.log"

    # The engine daemon runs on a Rust thread; when daemon_stop shuts
    # it down the thread ends but this wrapper process would linger.
    # Poll the socket and exit once the daemon stops answering, so the
    # service process lifecycle tracks the daemon itself.
    cmd = [
        sys.executable,
        "-c",
        "import socket, time\n"
        "from terra.daemon import Daemon\n"
        "d = Daemon(tcp=%r, embedded=False).start()\n"
        "print(d.socket, flush=True)\n"
        "while True:\n"
        "    time.sleep(0.5)\n"
        "    try:\n"
        "        s = socket.socket(socket.AF_UNIX)\n"
        "        s.settimeout(1)\n"
        "        s.connect(d.socket)\n"
        "        s.close()\n"
        "    except OSError:\n"
        "        break\n" % (args.tcp,),
    ]

    # When running via sudo, pass the original user's HOME so
    # the daemon finds images/layers in the right place.
    env = os.environ.copy()
    sudo_user = os.environ.get("SUDO_USER")
    if sudo_user:
        import pwd
        try:
            env["HOME"] = pwd.getpwnam(sudo_user).pw_dir
        except KeyError:
            pass

    try:
        log_fh = open(str(log_file), "a")
        stdout_target = log_fh
        stderr_target = log_fh
    except OSError:
        stdout_target = subprocess.DEVNULL
        stderr_target = subprocess.DEVNULL

    proc = subprocess.Popen(
        cmd, stdout=stdout_target, stderr=stderr_target, start_new_session=True, env=env
    )
    (paths.run_dir() / "daemon.pid").write_text(str(proc.pid))
    time.sleep(1.5)
    sock = paths.default_socket()
    print(f"daemon started (pid={proc.pid}, socket={sock})")
    return EXIT_OK


def cmd_daemon_status(args) -> int:
    """Show daemon status (renamed from ls)."""
    info = {"socket": paths.default_socket()}
    info["alive"] = _daemon_alive(info["socket"])
    info["pids"] = _daemon_pids()  # pidfile hint, not proof of liveness
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
    """Stop daemon gracefully via the daemon_stop wire command."""
    sock = paths.default_socket()
    if not _daemon_alive(sock):
        _daemon_pidfile().unlink(missing_ok=True)
        return _err(
            "No engine daemon running",
            cause=f"No daemon answering at {sock}",
            fix="If you want to ensure a clean state, try: terra daemon destroy",
        )
    try:
        TerraClient(socket_path=sock)._send({"command": "daemon_stop"})
    except TerraError as e:
        return _err(f"daemon_stop failed: {e}")
    except (OSError, TimeoutError) as e:
        return _err(f"could not reach daemon at {sock}: {e}")
    print("daemon stopped")
    # Wait for the daemon process to exit, then clean up hints.
    for pid in _daemon_pids():
        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                gone = Path(f"/proc/{pid}/stat").read_text().split()[2] == "Z"
            except (OSError, IndexError):
                gone = True
            if gone:
                break
            time.sleep(0.1)
    _daemon_pidfile().unlink(missing_ok=True)
    try:
        Path(sock).unlink()
    except FileNotFoundError:
        pass
    return EXIT_OK


def cmd_daemon_destroy(args) -> int:
    """Force-stop daemon (SIGKILL, pidfile hint)."""
    pids = _daemon_pids()
    if not pids:
        _daemon_pidfile().unlink(missing_ok=True)
        try:
            Path(paths.default_socket()).unlink()
        except FileNotFoundError:
            pass
        return _err(
            "No engine daemon running",
            cause="No daemon process or PID file found",
            fix="Nothing to destroy — the daemon is not running",
        )
    for pid in pids:
        os.kill(pid, _signal.SIGKILL)
        print(f"sent SIGKILL to daemon (pid={pid})")
    _daemon_pidfile().unlink(missing_ok=True)
    try:
        Path(paths.default_socket()).unlink()
    except FileNotFoundError:
        pass
    return EXIT_OK


# ── daemon internals ──────────────────────────────────────────────

def _daemon_pidfile() -> Path:
    return paths.run_dir() / "daemon.pid"


def _daemon_alive(socket_path: str) -> bool:
    """Liveness = the daemon answers a lightweight command on its socket.

    Neither pidfile existence nor a stale socket file proves the
    daemon is up; a reply to ``list`` does.
    """
    try:
        TerraClient(socket_path=socket_path)._send({"command": "list"})
        return True
    except TerraError:
        return True  # an error reply still proves the daemon is up
    except (OSError, TimeoutError, ValueError):
        return False


def _daemon_pids() -> list[int]:
    """Daemon pids from the pidfile — a hint for which process to
    signal/wait on, NOT a liveness check (see _daemon_alive)."""
    pidfile = _daemon_pidfile()
    if pidfile.exists():
        try:
            pid = int(pidfile.read_text().strip())
            stat = Path(f"/proc/{pid}/stat").read_text().split()[2]
            if stat != "Z":
                return [pid]
        except (OSError, ValueError, IndexError):
            pass
    return []


# ═══════════════════════════════════════════════════════════════════
# main
# ═══════════════════════════════════════════════════════════════════

def main() -> int:
    p = argparse.ArgumentParser(
        prog="terra",
        description="Terrarium CLI (python -m terra)",
        epilog="""\
Common workflows:
  First time setup (one command):
    terra setup alpine

  Quick sandbox:
    terra daemon start
    terra sandbox create --template alpine --net
    terra sandbox exec <vm-name> -- python3 --version
    terra sandbox kill <vm-name>

  Warm pool:
    terra pool create -n mypool --size 3
    terra pool claim --template alpine

  Direct VM:
    terra vm create dev --kernel default --layers base --net
    terra vm exec dev -- python3 --version
    terra vm destroy dev
""",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--socket", help="daemon socket path or tcp://host:port")
    p.add_argument("--json", action="store_true", help="machine-readable JSON output")
    p.add_argument("-v", "--verbose", action="store_true", help="verbose output (--json style for dicts)")
    sub = p.add_subparsers(dest="cmd", required=True)

    # ── setup ──────────────────────────────────────────────────────
    stp = sub.add_parser(
        "setup",
        help="one-command environment setup (kernel + rootfs + layers + template)",
    )
    stp.add_argument("distro", nargs="?", default="alpine", choices=["alpine", "ubuntu"],
                     help="base distro (default: alpine)")
    stp.add_argument("--kernel-version", default="6.12", help="kernel version (default: 6.12)")
    stp.add_argument("--force", action="store_true", help="rebuild even if artifacts exist")
    stp.set_defaults(f=cmd_setup)

    # ── sandbox ────────────────────────────────────────────────────
    sb = sub.add_parser("sandbox", help="high-level sandbox operations (recommended)")
    sbs = sb.add_subparsers(dest="action", required=True)

    sp = sbs.add_parser("create", help="create a sandbox from a template")
    sp.add_argument("--template", help="template name")
    sp.add_argument("--layers", nargs="*", help="explicit layer names (comma-separated)")
    sp.add_argument("--kernel", help="kernel variant name or path")
    sp.add_argument("--cpu", type=int, default=1, help="vCPU count (default: 1)")
    sp.add_argument("--memory", type=int, default=256, metavar="MB", help="memory in MiB (default: 256)")
    sp.add_argument("--net", action="store_true", help="attach NAT networking")
    sp.add_argument("--no-pool", action="store_true",
                    help="force a cold-booted dedicated tenant VM instead of claiming from the warm pool")
    sp.add_argument("--env", nargs="*", help="environment variables (KEY=VALUE)")
    sp.add_argument("--timeout", type=int, default=600, help="default command timeout (seconds)")
    sp.add_argument("--read-path", action="append", metavar="PATH",
                    help="exec policy: extra read-only grant (repeatable)")
    sp.add_argument("--write-path", action="append", metavar="PATH",
                    help="exec policy: extra read-write grant (repeatable)")
    sp.add_argument("--net-allow", action="append", metavar="HOST[:PORT]",
                    help="exec policy: deny-by-default egress except these (repeatable)")
    sp.add_argument("--memory-mb", type=int, metavar="N",
                    help="exec policy: per-exec memory limit in MiB")
    sp.add_argument("--procs", type=int, metavar="N",
                    help="exec policy: per-exec process-count limit")
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
    sp.add_argument("--no-sandbox", action="store_true",
                    help="run without sandlock permission isolation")
    sp.add_argument("--net-allow", action="append", metavar="HOST[:PORT]",
                    help="per-call exec policy override: deny-by-default egress except these (repeatable)")
    sp.add_argument("--detach", action="store_true",
                    help="run in the background — return a session_id immediately (poll with 'sandbox session status')")
    sp.add_argument("args", nargs="*", help="command and arguments (after --)")
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

    sp = sbs.add_parser("destroy-tenant", help="destroy a tenant VM and all its sandboxes")
    sp.add_argument("tenant", help="tenant identifier")
    sp.set_defaults(f=cmd_sandbox_destroy_tenant)

    sp = sbs.add_parser("session", help="background exec sessions (from sandbox exec --detach)")
    ss = sp.add_subparsers(dest="session_action", required=True)

    sp2 = ss.add_parser("status", help="show a background session's status")
    sp2.add_argument("session_id", help="background session ID")
    sp2.set_defaults(f=cmd_session_status)

    sp2 = ss.add_parser("kill", help="kill a background session")
    sp2.add_argument("session_id", help="background session ID")
    sp2.set_defaults(f=cmd_session_kill)

    ss.add_parser("ls", help="list background sessions").set_defaults(f=cmd_session_ls)

    # ── tool ────────────────────────────────────────────────────────
    tl = sub.add_parser("tool", help="tool layers built on distro templates")
    tls = tl.add_subparsers(dest="action", required=True)

    sp = tls.add_parser("create", help="build a tool layer in a builder VM")
    sp.add_argument("-n", "--name", required=True, help="tool layer name")
    sp.add_argument("--template", required=True,
                    help="distro template to build on (created by terra setup)")
    sp.add_argument("--script", help="provisioning script run inside the builder VM")
    sp.add_argument("--no-net", action="store_true", help="disable networking for builder VM")
    sp.add_argument("--timeout", type=int, default=600, help="builder VM timeout")
    sp.set_defaults(f=cmd_tool_create)

    tls.add_parser("ls", help="list tool layers").set_defaults(f=cmd_tool_ls)

    sp = tls.add_parser("remove", help="remove a tool layer")
    sp.add_argument("-n", "--name", required=True, help="tool layer name")
    sp.set_defaults(f=cmd_tool_remove)

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
    sp.add_argument("args", nargs="*", help="command and arguments (after --)")
    sp.set_defaults(f=cmd_vm_exec)

    sp = vms.add_parser("resize", help="resize VM resources")
    sp.add_argument("name", help="VM name")
    sp.add_argument("--cpus", type=int, help="new vCPU count")
    sp.add_argument("--memory-bytes", type=int, help="new memory in bytes")
    sp.set_defaults(f=cmd_vm_resize)

    sp = vms.add_parser("snapshot", help="capture a VM state for fast reset (P1)")
    sp.add_argument("name", help="VM name")
    sp.add_argument("--path", help="snapshot directory (default: {snapshot_dir}/terra-snap-<vm>)")
    sp.set_defaults(f=cmd_vm_snapshot)

    sp = vms.add_parser("restore", help="create a VM from a snapshot (P1 fast reset)")
    sp.add_argument("name", help="new VM name")
    sp.add_argument("--snapshot", required=True, help="snapshot directory (from terra vm snapshot)")
    sp.add_argument("--layers", nargs="*", default=[], help="layer names (comma-separated)")
    sp.add_argument("--net", action="store_true", help="attach NAT networking")
    sp.set_defaults(f=cmd_vm_restore)

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

    # ── audit ────────────────────────────────────────────────────────
    audit = sub.add_parser("audit", help="audit observability (P2)")
    audits = audit.add_subparsers(dest="action", required=True)
    sp = audits.add_parser("ls", help="query the audit ring buffer")
    sp.add_argument("--limit", type=int, help="max records (default 100)")
    sp.add_argument("--event", choices=["exec", "deny", "resource"], help="event kind filter")
    sp.add_argument("--id", help="sandbox id / vm name filter")
    sp.add_argument("--raw", action="store_true", help="print raw JSON")
    sp.set_defaults(f=cmd_audit_ls)

    # ── pool ────────────────────────────────────────────────────────
    pool = sub.add_parser("pool", help="warm pool operations")
    pools = pool.add_subparsers(dest="action", required=True)

    pools.add_parser("ls", help="list pools").set_defaults(f=cmd_pool_ls)

    sp = pools.add_parser("create", help="create a warm pool")
    sp.add_argument("--size", type=int, default=1, help="number of idle VMs")
    sp.add_argument("--kernel", help="kernel variant")
    sp.add_argument("--net", action="store_true", help="enable NAT networking")
    sp.set_defaults(f=cmd_pool_create)

    sp = pools.add_parser("claim", help="claim an idle pool VM")
    sp.add_argument("--template", help="template name for layers")
    sp.add_argument("--layers", nargs="+", help="explicit layers (comma-separated)")
    sp.set_defaults(f=cmd_pool_claim)

    sp = pools.add_parser("release", help="release a claimed VM back to idle")
    sp.add_argument("name", help="VM name")
    sp.set_defaults(f=cmd_pool_release)

    sp = pools.add_parser("scale", help="scale pool to new size")
    sp.add_argument("--size", type=int, required=True, help="new pool size")
    sp.add_argument("--kernel", help="kernel variant for new VMs")
    sp.add_argument("--net", action="store_true", help="enable NAT networking for new VMs")
    sp.set_defaults(f=cmd_pool_scale)

    sp = pools.add_parser("remove", help="remove a pool VM")
    sp.add_argument("name", help="VM name")
    sp.set_defaults(f=cmd_pool_remove)

    # ── net ─────────────────────────────────────────────────────────
    net = sub.add_parser("net", help="NAT networking")
    nets = net.add_subparsers(dest="action", required=True)

    nets.add_parser("ls", help="list networks").set_defaults(f=cmd_net_ls)

    sp = nets.add_parser("create", help="create NAT network")
    sp.set_defaults(f=cmd_net_create)

    sp = nets.add_parser("remove", help="remove NAT network")
    sp.add_argument("name", help="network name")
    sp.set_defaults(f=cmd_net_remove)

    # ── daemon ──────────────────────────────────────────────────────
    dmn = sub.add_parser("daemon", help="engine daemon lifecycle")
    dmns = dmn.add_subparsers(dest="action", required=True)

    sp = dmns.add_parser("start", help="start the daemon (detached background process)")
    sp.add_argument("--no-root", action="store_true",
                    help="run unprivileged (no sudo self-elevation; --net VMs unavailable)")
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
    except ValueError as e:  # e.g. client-side policy validation
        return _err(str(e), exit_code=EXIT_USAGE)
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

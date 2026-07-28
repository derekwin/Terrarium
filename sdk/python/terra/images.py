"""Guest image preparation — kernel, initramfs, rootfs, layers.

Two modes:
- repo checkout present: drive the images/ build scripts (authoritative)
- pip-only install: download prebuilt artifacts from $TERRA_ARTIFACT_BASE
  (published releases will host these; see ADR for URLs)
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import urllib.request
from pathlib import Path

from . import assets, paths

_ARTIFACTS = {
    "vmlinux.bin": "vmlinux.bin",
    "alpine.cpio": "alpine.cpio",
    "initramfs-virtiofs.cpio.gz": "initramfs-virtiofs.cpio.gz",
    "initramfs-agent.cpio.gz": "initramfs-agent.cpio.gz",
}

_BUILDERS = {
    "vmlinux.bin": "images/build-kernel.sh",
    "alpine.cpio": "images/build-rootfs.sh",
    "initramfs-virtiofs.cpio.gz": "images/build-initramfs-virtiofs.sh",
    "initramfs-agent.cpio.gz": "images/build-initramfs-agent.sh",
}


class ImageError(RuntimeError):
    """A guest image could not be provided."""


def _find_repo() -> Path | None:
    """Locate a Terrarium repo checkout (has images/build.sh)."""
    for base in (Path.cwd(), *Path.cwd().parents):
        if (base / "images" / "build.sh").exists():
            return base
    return None


def ensure(name: str) -> Path:
    """Ensure a guest image by name, return its path.

    Order: managed images dir -> repo target/guest -> repo build ->
    artifact download (TERRA_ARTIFACT_BASE).
    """
    if name not in _ARTIFACTS:
        raise ImageError(f"unknown image {name!r}; known: {sorted(_ARTIFACTS)}")

    managed = _migrate_artifact(name)

    repo = _find_repo()
    if repo:
        built = repo / "target" / "guest" / name
        if not built.exists():
            builder = repo / _BUILDERS[name]
            if not builder.exists():
                raise ImageError(f"builder missing: {builder}")
            if name in ("initramfs-virtiofs.cpio.gz", "initramfs-agent.cpio.gz"):
                _build_initramfs_via_rust(repo, name, built)
            else:
                subprocess.run(["bash", str(builder)], cwd=repo, check=True)
        # Refresh the managed copy when the repo image is newer —
        # stale images silently miss later fixes (learned the hard way).
        if not managed.exists() or built.stat().st_mtime > managed.stat().st_mtime:
            shutil.copy(built, managed)
        return managed

    if managed.exists():
        return managed

    base_url = os.environ.get("TERRA_ARTIFACT_BASE", "").rstrip("/")
    if base_url:
        try:
            urllib.request.urlretrieve(f"{base_url}/{name}", managed)
            return managed
        except Exception as e:  # noqa: BLE001
            managed.unlink(missing_ok=True)
            raise ImageError(f"download {name} from {base_url} failed: {e}") from e

    raise ImageError(
        f"guest image {name!r} not found: run images/build.sh from the "
        "Terrarium repo, or set TERRA_ARTIFACT_BASE to a prebuilt-artifacts URL"
    )


def ensure_all() -> dict[str, Path]:
    """Ensure all standard guest images."""
    return {name: ensure(name) for name in _ARTIFACTS}


def _migrate_artifact(name: str) -> Path:
    """Managed path for an image, migrating legacy flat layouts.

    kernels live at images/kernels/<name>/vmlinux.bin;
    rootfs/initramfs at images/rootfs/<name>.
    """
    if name == "vmlinux.bin":
        dest = paths.kernels_dir() / "default" / "vmlinux.bin"
        for legacy in (
            paths.images_dir() / "vmlinux.bin",
            paths.images_dir() / "default" / "vmlinux.bin",
        ):
            if legacy.exists() and not dest.exists():
                dest.parent.mkdir(parents=True, exist_ok=True)
                legacy.replace(dest)
        # migrate older variant dirs (images/<name>/vmlinux.bin)
        for d in paths.images_dir().iterdir():
            if d.is_dir() and (d / "vmlinux.bin").exists():
                target = paths.kernels_dir() / d.name / "vmlinux.bin"
                if not target.exists():
                    target.parent.mkdir(parents=True, exist_ok=True)
                    (d / "vmlinux.bin").replace(target)
                try:
                    d.rmdir()
                except OSError:
                    pass
        return dest
    dest = paths.rootfs_dir() / name
    legacy = paths.images_dir() / name
    if legacy.exists() and not dest.exists():
        legacy.replace(dest)
    return dest


def resolve_kernel(name_or_path: str) -> Path:
    """Resolve a kernel variant name or explicit path to a vmlinux.bin.

    Convention: images/kernels/<name>/vmlinux.bin.
    """
    p = Path(name_or_path).expanduser()
    if p.exists():
        return p
    variant = paths.kernels_dir() / name_or_path / "vmlinux.bin"
    if variant.exists():
        return variant
    if name_or_path == "default":
        return _migrate_artifact("vmlinux.bin")
    raise ImageError(
        f"kernel variant {name_or_path!r} not found — build one with "
        f"`terra kernel create -n {name_or_path} --version <ver>`"
    )


def resolve_rootfs(name_or_path: str) -> Path:
    """Resolve a rootfs/initramfs image by variant name or explicit path.

    Convention: images/rootfs/<name>, <name>.cpio, <name>.cpio.gz.
    Well-known aliases: alpine, virtiofs, agent.
    """
    p = Path(name_or_path).expanduser()
    if p.exists():
        return p
    aliases = {
        "alpine": "alpine.cpio",
        "virtiofs": "initramfs-virtiofs.cpio.gz",
        "agent": "initramfs-agent.cpio.gz",
    }
    name = aliases.get(name_or_path, name_or_path)
    for cand in (
        paths.rootfs_dir() / name,
        paths.rootfs_dir() / f"{name}.cpio",
        paths.rootfs_dir() / f"{name}.cpio.gz",
    ):
        if cand.exists():
            return cand
    raise ImageError(
        f"rootfs image {name_or_path!r} not found under {paths.rootfs_dir()} "
        f"(see `terra rootfs ls`)"
    )


def _build_initramfs_via_rust(repo: Path, name: str, output: Path) -> None:
    """Build an initramfs using terrarium_fs (replaces shell scripts)."""
    import terrarium_fs

    src_rootfs = _ensure_src_rootfs(repo)

    if name == "initramfs-agent.cpio.gz":
        gp = _ensure_guest_proxy(repo)
        init = str(repo / "images" / "rootfs" / "init-agent")
        output.parent.mkdir(parents=True, exist_ok=True)
        terrarium_fs.build_initramfs_agent(src_rootfs, gp, init, str(output))
    elif name == "initramfs-virtiofs.cpio.gz":
        init = str(repo / "images" / "rootfs" / "init-virtiofs")
        output.parent.mkdir(parents=True, exist_ok=True)
        terrarium_fs.build_initramfs_virtiofs(src_rootfs, init, str(output))


def _ensure_src_rootfs(repo: Path) -> str:
    """Return a directory with bin/busybox and musl libs.

    Uses target/guest/rootfs if it exists; otherwise extracts alpine.cpio
    to a temp dir.
    """
    rootfs_dir = repo / "target" / "guest" / "rootfs"
    if (rootfs_dir / "bin" / "busybox").exists():
        return str(rootfs_dir)

    alpine = repo / "target" / "guest" / "alpine.cpio"
    if not alpine.exists():
        raise ImageError(
            f"no rootfs source: need {rootfs_dir} or {alpine} "
            f"(run build-rootfs.sh first)"
        )

    extract_dir = tempfile.mkdtemp(prefix="terrarium-src-")
    # Extract alpine.cpio — may be gzipped or raw.
    cmd = (
        f"zcat '{alpine}' 2>/dev/null || cat '{alpine}'"
        f" | (cd '{extract_dir}' && cpio -idm --quiet)"
    )
    subprocess.run(cmd, shell=True, check=True)
    return extract_dir


def _ensure_guest_proxy(repo: Path) -> str:
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


def build_layer(src_dir: str, name: str) -> Path:
    """Pack a directory into an EROFS layer image in the managed layers dir."""
    import terrarium_fs

    layers_dir = str(paths.layers_dir())
    return Path(terrarium_fs.build_erofs_layer(str(src_dir), name, layers_dir))

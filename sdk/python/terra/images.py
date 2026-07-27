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

    managed = paths.images_dir() / name

    repo = _find_repo()
    if repo:
        built = repo / "target" / "guest" / name
        if not built.exists():
            builder = repo / _BUILDERS[name]
            if not builder.exists():
                raise ImageError(f"builder missing: {builder}")
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


def resolve_kernel(name_or_path: str) -> Path:
    """Resolve a kernel variant name or explicit path to a vmlinux.bin.

    Convention: images/<name>/vmlinux.bin. A bare images/vmlinux.bin
    (legacy) is migrated to images/default/vmlinux.bin on first touch.
    """
    p = Path(name_or_path).expanduser()
    if p.exists():
        return p
    default = paths.images_dir() / "default" / "vmlinux.bin"
    legacy = paths.images_dir() / "vmlinux.bin"
    if legacy.exists() and not default.exists():
        default.parent.mkdir(parents=True, exist_ok=True)
        legacy.replace(default)
    variant = paths.images_dir() / name_or_path / "vmlinux.bin"
    if variant.exists():
        return variant
    if name_or_path == "default" and default.exists():
        return default
    raise ImageError(
        f"kernel variant {name_or_path!r} not found — build one with "
        f"`terra kernel create -n {name_or_path} --version <ver>`"
    )


def build_layer(src_dir: str, name: str) -> Path:
    """Pack a directory into an EROFS layer image in the managed layers dir."""
    mkfs, _fuse = assets.ensure_erofs_tools()
    out = paths.layers_dir() / f"{name}.erofs"
    tmp = out.with_suffix(".tmp")
    subprocess.run(
        [str(mkfs), "-zlz4", str(tmp), str(Path(src_dir).resolve()) + "/"],
        check=True, capture_output=True,
    )
    tmp.replace(out)
    return out

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
    if managed.exists():
        return managed

    repo = _find_repo()
    if repo:
        built = repo / "target" / "guest" / name
        if not built.exists():
            builder = repo / _BUILDERS[name]
            if not builder.exists():
                raise ImageError(f"builder missing: {builder}")
            subprocess.run(["bash", str(builder)], cwd=repo, check=True)
        shutil.copy(built, managed)
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

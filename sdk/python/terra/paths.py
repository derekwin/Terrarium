"""Managed directory layout for the Terrarium SDK.

Everything the SDK downloads or builds lives under one root so users
never have to place binaries or set environment variables:

    ~/.local/share/terra/            (or $TERRA_HOME)
    ├── bin/        cloud-hypervisor, virtiofsd, mkfs.erofs, erofsfuse
    ├── images/     vmlinux.bin, *.cpio.gz guest images
    ├── layers/     filesystem layers (dirs and .erofs images)
    ├── state/      daemon state (fs workdirs, overlays)
    └── run/        terra.sock and other runtime sockets
"""

from __future__ import annotations

import os
from pathlib import Path

_ROOT: Path | None = None


def _default_root() -> Path:
    """The managed root for the current process.

    Follows the service-data convention (docker: /var/lib/docker):
    the default is a system-wide shared data directory (/var/lib/terra)
    created by installation, so a root daemon and every user's CLI see
    one consistent asset tree. A non-root user on a machine without the
    system install falls back to their own ~/.local/share/terra
    (zero-config development). TERRA_HOME overrides everything.
    """
    if os.geteuid() == 0 or _writable(Path("/var/lib/terra")):
        return Path("/var/lib/terra")
    xdg = os.environ.get("XDG_DATA_HOME")
    base = Path(xdg) if xdg else Path.home() / ".local" / "share"
    return base / "terra"


def _writable(p: Path) -> bool:
    try:
        return os.access(p, os.W_OK)
    except OSError:
        return False


def root() -> Path:
    """Managed root: $TERRA_HOME or ~/.local/share/terra (created lazily)."""
    global _ROOT
    if _ROOT is None:
        env = os.environ.get("TERRA_HOME")
        if env:
            _ROOT = Path(env)
        else:
            _ROOT = _default_root()
    _ROOT.mkdir(parents=True, exist_ok=True)
    return _ROOT


def bin_dir() -> Path:
    """Downloaded/managed binaries (CH, virtiofsd, erofs tools)."""
    d = root() / "bin"
    d.mkdir(parents=True, exist_ok=True)
    return d


def images_dir() -> Path:
    """Guest kernel and initramfs images."""
    d = root() / "images"
    d.mkdir(parents=True, exist_ok=True)
    return d


def kernels_dir() -> Path:
    """Kernel variants: images/kernels/<name>/vmlinux.bin."""
    d = images_dir() / "kernels"
    d.mkdir(parents=True, exist_ok=True)
    return d


def rootfs_dir() -> Path:
    """Rootfs/initramfs images: images/rootfs/..."""
    d = images_dir() / "rootfs"
    d.mkdir(parents=True, exist_ok=True)
    return d


def layers_dir() -> Path:
    """Filesystem layers (dirs and .erofs images)."""
    d = root() / "layers"
    d.mkdir(parents=True, exist_ok=True)
    return d


def state_dir() -> Path:
    """Daemon state (fs workdirs etc)."""
    d = root() / "state"
    d.mkdir(parents=True, exist_ok=True)
    return d


def run_dir() -> Path:
    """Runtime sockets."""
    d = root() / "run"
    d.mkdir(parents=True, exist_ok=True)
    return d


def default_socket() -> str:
    """Default engine daemon socket path — always /tmp/terra.sock."""
    return "/tmp/terra.sock"

"""Asset management — automatic download/install of host dependencies.

Users should never have to install or place binaries manually. The SDK
fetches everything it needs into the managed bin dir (see paths.py):

- cloud-hypervisor : official static release binary (GitHub)
- virtiofsd        : qemu's build via apt download+extract (no sudo),
                     or rust-vmm's via `cargo install`
- mkfs.erofs/erofsfuse : erofs-utils / erofsfuse debs via apt extract

All `ensure_*` functions are idempotent and return the binary path.
"""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import urllib.request
from pathlib import Path

from . import paths

CH_VERSION = "53.0"
CH_URL = (
    "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/"
    f"download/v{CH_VERSION}/cloud-hypervisor-static"
)

# apt packages and the member binary we extract from each.
_APT_PACKAGES = {
    "virtiofsd": ("qemu-utils", "usr/lib/qemu/virtiofsd"),
    "mkfs.erofs": ("erofs-utils", "usr/bin/mkfs.erofs"),
    "erofsfuse": ("erofsfuse", "usr/bin/erofsfuse"),
}


class AssetError(RuntimeError):
    """An asset could not be provided."""


def _chmod_x(path: Path) -> None:
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _download(url: str, dest: Path) -> None:
    tmp = dest.with_suffix(dest.suffix + ".tmp")
    try:
        urllib.request.urlretrieve(url, tmp)
        _chmod_x(tmp)
        tmp.replace(dest)
    except Exception as e:  # noqa: BLE001
        tmp.unlink(missing_ok=True)
        raise AssetError(f"download {url} failed: {e}") from e


def ensure_ch(version: str = CH_VERSION) -> Path:
    """Ensure a cloud-hypervisor binary, return its path."""
    env = os.environ.get("TERRA_CH_BINARY")
    if env and Path(env).exists():
        return Path(env)
    found = shutil.which("cloud-hypervisor")
    if found:
        return Path(found)
    dest = paths.bin_dir() / "cloud-hypervisor"
    if not dest.exists():
        url = CH_URL if version == CH_VERSION else CH_URL.replace(CH_VERSION, version)
        _download(url, dest)
    return dest


def _apt_extract(pkg: str, member: str) -> Path | None:
    """Download a .deb via apt (no sudo) and extract one binary."""
    if not shutil.which("apt"):
        return None
    try:
        with tempfile.TemporaryDirectory() as td:
            subprocess.run(
                ["apt", "download", pkg],
                cwd=td, check=True, capture_output=True, timeout=300,
            )
            deb = next(Path(td).glob("*.deb"))
            out = Path(td) / "x"
            subprocess.run(["dpkg", "-x", str(deb), str(out)], check=True, capture_output=True)
            src = out / member
            if not src.exists():
                return None
            dest = paths.bin_dir() / Path(member).name
            shutil.copy(src, dest)
            _chmod_x(dest)
            return dest
    except Exception:  # noqa: BLE001
        return None


def _cargo_install(crate: str, bin_name: str) -> Path | None:
    if not shutil.which("cargo"):
        return None
    try:
        subprocess.run(
            [
                "cargo", "install", crate, "--locked",
                "--root", str(paths.bin_dir().parent / "cargo"),
            ],
            check=True, capture_output=True, timeout=1800,
        )
        dest = paths.bin_dir().parent / "cargo" / "bin" / bin_name
        if dest.exists():
            link = paths.bin_dir() / bin_name
            if not link.exists():
                link.symlink_to(dest)
            return link
    except Exception:  # noqa: BLE001
        pass
    return None


def ensure_virtiofsd() -> Path:
    """Ensure a virtiofsd binary (qemu or rust-vmm flavor)."""
    env = os.environ.get("TERRA_VIRTIOFSD")
    if env and Path(env).exists():
        return Path(env)
    for c in (shutil.which("virtiofsd"), str(Path.home() / ".cargo/bin/virtiofsd"), "/usr/lib/qemu/virtiofsd"):
        if c and Path(c).exists():
            return Path(c)
    dest = paths.bin_dir() / "virtiofsd"
    if dest.exists():
        return dest
    got = _apt_extract(*_APT_PACKAGES["virtiofsd"])
    if got:
        return got
    got = _cargo_install("virtiofsd", "virtiofsd")
    if got:
        return got
    raise AssetError(
        "virtiofsd unavailable: install it (apt install qemu-utils / cargo install virtiofsd) "
        "or set TERRA_VIRTIOFSD"
    )


def ensure_erofs_tools() -> tuple[Path, Path]:
    """Ensure (mkfs.erofs, erofsfuse) binaries."""
    mkfs = shutil.which("mkfs.erofs")
    fuse = shutil.which("erofsfuse")
    if mkfs and fuse:
        return Path(mkfs), Path(fuse)
    mkfs_p = paths.bin_dir() / "mkfs.erofs"
    fuse_p = paths.bin_dir() / "erofsfuse"
    if not mkfs_p.exists():
        got = _apt_extract(*_APT_PACKAGES["mkfs.erofs"])
        if not got:
            raise AssetError("mkfs.erofs unavailable (apt install erofs-utils)")
    if not fuse_p.exists():
        got = _apt_extract(*_APT_PACKAGES["erofsfuse"])
        if not got:
            raise AssetError("erofsfuse unavailable (apt install erofsfuse)")
    return mkfs_p, fuse_p

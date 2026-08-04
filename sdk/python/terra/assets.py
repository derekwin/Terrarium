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

# apt packages and the member file we extract from each. When a binary
# needs a runtime library (mkfs.erofs -> libdeflate.so.0), list both so
# they are pulled together into the managed bin dir — the SDK stays
# self-contained instead of depending on a host package.
_APT_PACKAGES = {
    "virtiofsd": ("qemu-utils", "usr/lib/qemu/virtiofsd"),
    "mkfs.erofs": ("erofs-utils", "usr/bin/mkfs.erofs"),
    "erofsfuse": ("erofsfuse", "usr/bin/erofsfuse"),
    "libdeflate": ("libdeflate0", "usr/lib/x86_64-linux-gnu/libdeflate.so.0"),
}


class AssetError(RuntimeError):
    """An asset could not be provided."""


# Set once apt-get update has succeeded in this process; subsequent
# downloads don't need (or pay for) another refresh.
_APT_LISTS_REFRESHED = False


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


def _apt_download(pkg: str, cwd: Path) -> None:
    """Download one .deb with apt; refresh package lists on first miss."""
    global _APT_LISTS_REFRESHED
    try:
        subprocess.run(
            ["apt", "download", pkg],
            cwd=cwd, check=True, capture_output=True, timeout=300,
        )
    except subprocess.CalledProcessError:
        # Fresh hosts have no apt lists yet; refresh once, then retry.
        if not _APT_LISTS_REFRESHED:
            subprocess.run(
                ["apt-get", "update", "-qq"],
                check=True, capture_output=True, timeout=600,
            )
            _APT_LISTS_REFRESHED = True
        subprocess.run(
            ["apt", "download", pkg],
            cwd=cwd, check=True, capture_output=True, timeout=300,
        )


def _apt_extract(*members: tuple[str, str]) -> dict[str, Path]:
    """Download .debs via apt (no sudo) and extract the named members.

    Returns ``{member basename: managed bin path}`` for every member that
    was extracted. Multiple packages can be passed so a binary and its
    runtime libraries are pulled together (e.g. ``mkfs.erofs`` and the
    ``libdeflate0`` shared object it links against).
    """
    if not shutil.which("apt"):
        return {}
    try:
        with tempfile.TemporaryDirectory() as td:
            for pkg, _ in members:
                _apt_download(pkg, Path(td))
            out = Path(td) / "x"
            for deb in sorted(Path(td).glob("*.deb")):
                subprocess.run(["dpkg", "-x", str(deb), str(out)], check=True, capture_output=True)
            extracted: dict[str, Path] = {}
            for _, member in members:
                src = out / member
                if not src.exists():
                    continue
                dest = paths.bin_dir() / Path(member).name
                shutil.copy(src, dest)
                _chmod_x(dest)
                extracted[Path(member).name] = dest
            return extracted
    except Exception:  # noqa: BLE001
        return {}


def _bundled_libdeflate() -> Path:
    """Managed libdeflate.so.0 bundled next to mkfs.erofs."""
    return paths.bin_dir() / "libdeflate.so.0"


def _bundled_libfuse() -> Path:
    """Managed libfuse.so.2 bundled next to erofsfuse."""
    return paths.bin_dir() / "libfuse.so.2"


def _ensure_bundled_libfuse() -> None:
    """Fetch libfuse.so.2 for erofsfuse (fuse-based mount fallback).

    The package name differs across Debian/Ubuntu releases
    (libfuse2t64 on newer, libfuse2 on older) — try both.
    """
    if _bundled_libfuse().exists():
        return
    for pkg in ("libfuse2t64", "libfuse2"):
        got = _apt_extract((pkg, "usr/lib/x86_64-linux-gnu/libfuse.so.2"))
        if got:
            return


def _ensure_bin_loader_path() -> None:
    """Put the managed bin dir on the dynamic loader path.

    mkfs.erofs/erofsfuse are extracted from Debian packages and link
    against libdeflate.so.0, which is bundled next to them. LD_LIBRARY_PATH
    keeps the tools self-contained — no system libdeflate0 required.
    """
    bin_dir = str(paths.bin_dir())
    current = os.environ.get("LD_LIBRARY_PATH", "")
    entries = current.split(os.pathsep) if current else []
    if bin_dir not in entries:
        os.environ["LD_LIBRARY_PATH"] = os.pathsep.join([bin_dir, *entries]) if entries else bin_dir


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
    got = _apt_extract(_APT_PACKAGES["virtiofsd"])
    if "virtiofsd" in got:
        return got["virtiofsd"]
    got = _cargo_install("virtiofsd", "virtiofsd")
    if got:
        return got
    raise AssetError(
        "virtiofsd unavailable: install it (apt install qemu-utils / cargo install virtiofsd) "
        "or set TERRA_VIRTIOFSD"
    )


def ensure_erofs_tools() -> tuple[Path, Path]:
    """Ensure (mkfs.erofs, erofsfuse) binaries plus bundled runtime libs."""
    mkfs = shutil.which("mkfs.erofs")
    fuse = shutil.which("erofsfuse")
    if mkfs and fuse:
        managed_mkfs = paths.bin_dir() / "mkfs.erofs"
        if managed_mkfs.exists() and not _bundled_libdeflate().exists():
            # SDK-managed mkfs.erofs missing its bundled runtime lib
            # (e.g. installed before libdeflate0 was bundled) — fetch it.
            _apt_extract(_APT_PACKAGES["libdeflate"])
        _ensure_bundled_libfuse()
        _ensure_bin_loader_path()
        return Path(mkfs), Path(fuse)
    mkfs_p = paths.bin_dir() / "mkfs.erofs"
    fuse_p = paths.bin_dir() / "erofsfuse"
    if not mkfs_p.exists() or not _bundled_libdeflate().exists():
        got = _apt_extract(
            _APT_PACKAGES["mkfs.erofs"],
            _APT_PACKAGES["libdeflate"],
        )
        if not mkfs_p.exists() and "mkfs.erofs" not in got:
            raise AssetError("mkfs.erofs unavailable (apt install erofs-utils)")
    if not fuse_p.exists():
        got = _apt_extract(_APT_PACKAGES["erofsfuse"])
        if not got:
            raise AssetError("erofsfuse unavailable (apt install erofsfuse)")
    _ensure_bundled_libfuse()
    _ensure_bin_loader_path()
    return mkfs_p, fuse_p

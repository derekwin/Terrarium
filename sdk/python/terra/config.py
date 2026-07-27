"""HostConfig — declarative configuration for a daemon host.

The engine daemon is deliberately a thin runtime; everything a site
wants to decide — which kernel and images, which layers, pool size,
VM defaults, networking — is configuration, not code. A Python
"daemon program" is therefore just config + serve:

    from terra.daemon import Daemon, HostConfig

    cfg = HostConfig(
        kernel="~/images/vmlinux.bin",
        agent_initramfs="~/images/initramfs-agent.cpio.gz",
        layer_dir="~/layers",
        pool_size=4,
        default_net=True,
    )
    with Daemon(config=cfg):
        ...  # serve clients
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class HostConfig:
    """Site-level daemon configuration."""

    # Guest images
    kernel: str | None = None
    agent_initramfs: str | None = None
    virtiofs_initramfs: str | None = None
    # Directories
    layer_dir: str | None = None
    state_dir: str | None = None
    # Binaries (auto-resolved when None)
    ch_binary: str | None = None
    virtiofsd: str | None = None
    # Pool
    pool_size: int = 0
    # VM defaults
    default_cpus: int = 2
    default_memory_mb: int = 512
    default_layers: list[str] = field(default_factory=list)
    default_net: bool = False
    # Access
    token: str | None = None

    def env(self) -> dict[str, str]:
        """Environment variable mapping understood by the engine."""
        env: dict[str, str] = {}
        if self.kernel:
            env["TERRA_KERNEL"] = str(Path(self.kernel).expanduser())
        if self.agent_initramfs:
            env["TERRA_AGENT_INITRAMFS"] = str(
                Path(self.agent_initramfs).expanduser()
            )
        if self.layer_dir:
            env["TERRA_LAYER_DIR"] = str(Path(self.layer_dir).expanduser())
        if self.state_dir:
            env["TERRA_STATE_DIR"] = str(Path(self.state_dir).expanduser())
        if self.ch_binary:
            env["TERRA_CH_BINARY"] = self.ch_binary
        if self.virtiofsd:
            env["TERRA_VIRTIOFSD"] = self.virtiofsd
        if self.token:
            env["TERRA_TOKEN"] = self.token
        return env

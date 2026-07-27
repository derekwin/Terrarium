"""Direct mode — module-level API over a hidden default session.

For scripts and notebooks: no Daemon, no pool, no socket to think
about. The first call lazily starts a managed daemon and registers
cleanup at process exit.

    import terra
    vm = terra.create(layers=["python312", "base"])
    print(vm.exec(["python3", "--version"]))
    vm.destroy()

Pass `config=` once to customize the underlying host (images, layers,
pool size, ...); everything else is automatic.
"""

from __future__ import annotations

import atexit
import itertools
from pathlib import Path
import threading

from .client import TerraClient
from .config import HostConfig
from .daemon import Daemon
from . import images
from .vm import Vm

_lock = threading.Lock()
_daemon: Daemon | None = None
_client: TerraClient | None = None
_mode = "local"  # "local" | "remote"
_names = itertools.count(1)


def _start(config: HostConfig | None = None) -> TerraClient:
    global _daemon, _client
    with _lock:
        if _client is None:
            _daemon = Daemon(config=config).start()
            _client = TerraClient(socket_path=_daemon.socket)

            def _cleanup() -> None:
                try:
                    _daemon.stop()
                except Exception:  # noqa: BLE001
                    pass

            atexit.register(_cleanup)
        return _client


def configure(config: HostConfig) -> None:
    """Set host configuration before the first call."""
    global _client, _daemon
    if _client is not None:
        raise RuntimeError("default session already started — configure() must come first")
    _start(config)


def connect(address: str, token: str | None = None) -> None:
    """Point the default session at a (possibly remote) daemon.

    After this, terra.create()/exec()/destroy() run against that daemon
    with exactly the same code as local mode — creation is fulfilled by
    the daemon's warm pool (pool_claim) when layers are given.

        terra.connect("tcp://server:19099", token="secret")
        vm = terra.create(layers=["python312", "base"])
    """
    global _client, _mode
    with _lock:
        if _client is not None:
            raise RuntimeError("default session already started — connect() must come first")
        _client = TerraClient(address, token=token)
        _mode = "remote"


def client() -> TerraClient:
    """The shared default-session client (starts it on first call)."""
    return _client if _client is not None else _start()


def create(
    name: str | None = None,
    *,
    layers: list[str] | None = None,
    kernel: str | None = None,
    initramfs: str | None = None,
    cpus: int | None = None,
    memory_mb: int | None = None,
    net: bool | None = None,
) -> Vm:
    """Create a VM in the default session.

    Everything is optional: name auto-generates, kernel/initramfs
    resolve from managed images, resources use HostConfig defaults.
    """
    c = client()
    cfg = _daemon.config if _daemon else HostConfig()
    layers = layers if layers is not None else list(cfg.default_layers)
    if kernel is None:
        kernel = str(images.ensure("vmlinux.bin"))
    elif not Path(kernel).exists():
        kernel = str(images.resolve_kernel(kernel))
    if layers:
        initramfs = str(images.resolve_rootfs("virtiofs"))
    elif initramfs is None:
        initramfs = str(images.ensure("alpine.cpio"))
    elif not Path(initramfs).exists():
        initramfs = str(images.resolve_rootfs(initramfs))
    name = name or f"vm-{next(_names)}"
    if _mode == "remote" and layers:
        # Remote daemons allocate from the warm pool — same verb.
        claim = c.pool_claim(layers)
        return Vm(name=claim["name"], client=c, pid=claim.get("pid"), pooled=True)
    resp = c.vm_create(
        name,
        kernel,
        initramfs=initramfs,
        layers=layers or None,
        cpus=cpus or cfg.default_cpus,
        memory_mb=memory_mb or cfg.default_memory_mb,
        net=cfg.default_net if net is None else net,
    )
    return Vm(name=name, client=c, pid=resp.get("pid"))


def list_vms() -> list[Vm]:
    from .vm import list_vms as _list

    return _list(client())

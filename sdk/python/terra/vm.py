"""VM operations — the first-class VM object."""

from __future__ import annotations

from .client import TerraClient, TerraError

__all__ = ["Vm", "create", "list_vms", "TerraError"]


class Vm:
    """A running Terrarium VM."""

    def __init__(self, name: str, client: TerraClient, pid: int | None = None):
        self.name = name
        self._client = client
        self._pid = pid

    def __repr__(self) -> str:
        return f"Vm(name={self.name!r}, pid={self._pid})"

    @property
    def pid(self) -> int | None:
        return self._pid

    def info(self) -> dict:
        """Query VM state and resource usage. Returns unwrapped data dict."""
        return self._client.vm_info(self.name)

    def resize(
        self,
        *,
        cpus: int | None = None,
        memory_bytes: int | None = None,
    ) -> dict:
        """Resize CPU or memory online."""
        return self._client.vm_resize(
            self.name, cpus=cpus, memory_bytes=memory_bytes
        )

    def exec(self, args: list[str]) -> dict:
        """Execute a command inside the VM via the guest agent."""
        return self._client.vm_exec(self.name, args)

    def shutdown(self) -> dict:
        """Gracefully shut down and deregister the VM."""
        return self._client.vm_shutdown(self.name)

    def kill(self) -> dict:
        """Force-kill and deregister the VM."""
        return self._client.vm_kill(self.name)

    def destroy(self) -> dict:
        """Stop and deregister the VM."""
        return self._client.vm_destroy(self.name)

    def __enter__(self) -> "Vm":
        return self

    def __exit__(self, *args: object) -> None:
        self.shutdown()


def create(
    name: str,
    kernel: str,
    *,
    initramfs: str | None = None,
    cmdline: str | None = None,
    cpus: int = 2,
    max_cpus: int | None = 16,
    memory_mb: int = 512,
    max_memory_mb: int | None = None,
    layers: list[str] | None = None,
    client: TerraClient | None = None,
) -> Vm:
    """Create a new VM.

    Args:
        name: Unique VM name.
        kernel: Path to kernel image (bzImage).
        initramfs: Path to initramfs cpio archive (use
            target/guest/initramfs-virtiofs.cpio.gz when layers are set).
        layers: virtiofs layer names, highest priority first, base last.

    Raises:
        TerraError: If the engine returns an error.
    """
    if client is None:
        client = TerraClient()
    resp = client.vm_create(
        name=name,
        kernel=kernel,
        initramfs=initramfs,
        cmdline=cmdline,
        cpus=cpus,
        max_cpus=max_cpus,
        memory_mb=memory_mb,
        max_memory_mb=max_memory_mb,
        layers=layers,
    )
    return Vm(name=name, client=client, pid=resp.get("pid"))


def list_vms(client: TerraClient | None = None) -> list[Vm]:
    """List all running VMs.

    Raises:
        TerraError: If the engine returns an error.
    """
    if client is None:
        client = TerraClient()
    resp = client.vm_list()
    vms = []
    for item in resp.get("vms", []):
        vms.append(
            Vm(name=item["name"], client=client, pid=item.get("pid"))
        )
    return vms

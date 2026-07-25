"""VM operations — the first-class VM object."""

from __future__ import annotations

from .client import TerraClient


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
        """Query VM state and resource usage."""
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

    def shutdown(self) -> dict:
        """Gracefully shut down the VM. Overlay disk is kept."""
        return self._client.vm_shutdown(self.name)

    def kill(self) -> dict:
        """Force-kill the VM. Overlay disk is kept."""
        return self._client.vm_kill(self.name)

    def destroy(self) -> dict:
        """Shut down and delete the overlay disk permanently."""
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
    base_disk: str | None = None,
    disk_size_gb: int = 20,
    client: TerraClient | None = None,
) -> Vm:
    """Create a new VM.

    Args:
        name: Unique VM name.
        kernel: Path to kernel image (bzImage).
        initramfs: Path to initramfs cpio archive.
        base_disk: Base qcow2 for overlay (shared read-only).
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
        base_disk=base_disk,
        disk_size_gb=disk_size_gb,
    )
    if resp.get("status") != "ok":
        raise RuntimeError(f"VM create failed: {resp.get('error', resp)}")
    data = resp.get("data", {})
    return Vm(name=name, client=client, pid=data.get("pid"))


def list_vms(client: TerraClient | None = None) -> list[Vm]:
    """List all running VMs."""
    if client is None:
        client = TerraClient()
    resp = client.vm_list()
    if resp.get("status") != "ok":
        raise RuntimeError(f"VM list failed: {resp.get('error', resp)}")
    vms = []
    for item in resp.get("data", {}).get("vms", []):
        vms.append(
            Vm(name=item["name"], client=client, pid=item.get("pid"))
        )
    return vms

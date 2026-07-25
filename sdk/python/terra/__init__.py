"""Terrarium Engine Python SDK.

Usage:
    import terra

    # Create a VM
    vm = terra.vm.create("demo", kernel="target/guest/vmlinux.bin",
                         initramfs="target/guest/alpine-python.cpio")

    # Query VM info
    info = vm.info()

    # Resize VM
    vm.resize(cpus=4)

    # Cleanup
    vm.shutdown()

    # List all VMs
    vms = terra.vm.list_vms()
"""

from .client import TerraClient
from . import vm

__all__ = ["TerraClient", "vm"]

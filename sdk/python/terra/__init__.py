"""Terrarium Engine Python SDK.

Usage:
    import terra

    # Create a VM
    vm = terra.vm.create("demo", kernel="target/guest/vmlinux.bin",
                         initramfs="target/guest/alpine-python.cpio")

    # Create a sandbox inside the VM
    sb = terra.sandbox.create("agent-1", tools=["python"])

    # Execute commands
    result = sb.exec("python3", "-c", "print(2 ** 10)")
    print(result.stdout)  # 1024

    # Read agent output files
    content = sb.read_file("/home/agent/output.txt")

    # Cleanup
    vm.shutdown()
"""

from .client import TerraClient
from . import vm
from . import sandbox

__all__ = ["TerraClient", "vm", "sandbox"]

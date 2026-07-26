"""Low-level JSON client for the Terrarium engine daemon.

Communicates over a Unix domain socket with newline-delimited JSON.
"""

from __future__ import annotations

import json
import socket


class TerraError(Exception):
    """Raised when the engine daemon returns an error response."""

    def __init__(self, message: str):
        super().__init__(message)
        self.message = message


class TerraClient:
    """Client for the terrarium engine daemon."""

    def __init__(self, socket_path: str = "/tmp/terra.sock"):
        self.socket_path = socket_path

    def _send(self, cmd: dict) -> dict:
        """Send a JSON command and return the parsed response data."""
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(30)
        try:
            sock.connect(self.socket_path)
            payload = json.dumps(cmd) + "\n"
            sock.sendall(payload.encode())

            response = b""
            while True:
                try:
                    chunk = sock.recv(4096)
                    if not chunk:
                        break
                    response += chunk
                except socket.timeout:
                    sock.close()
                    raise TimeoutError("engine daemon did not respond within timeout") from None

            resp = json.loads(response.decode())
            if resp.get("status") == "error":
                raise TerraError(resp.get("error", "unknown error"))
            return resp.get("data", resp)
        finally:
            sock.close()

    def vm_create(
        self,
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
    ) -> dict:
        """Create a new VM.

        Args:
            layers: virtiofs layer names, highest priority first, base
                layer last. None = plain initramfs boot.
        """
        cmd = {"command": "create", "name": name, "kernel": kernel}
        if initramfs:
            cmd["initramfs"] = initramfs
        if cmdline:
            cmd["cmdline"] = cmdline
        cmd["cpus"] = cpus
        cmd["max_cpus"] = max_cpus
        cmd["memory_mb"] = memory_mb
        if max_memory_mb:
            cmd["max_memory_mb"] = max_memory_mb
        if layers:
            cmd["layers"] = list(layers)
        return self._send(cmd)

    def vm_list(self) -> dict:
        """List all running VMs."""
        return self._send({"command": "list"})

    def vm_info(self, name: str) -> dict:
        """Get VM details."""
        return self._send({"command": "info", "name": name})

    def vm_resize(
        self,
        name: str,
        *,
        cpus: int | None = None,
        memory_bytes: int | None = None,
    ) -> dict:
        """Resize VM resources."""
        cmd = {"command": "resize", "name": name}
        if cpus is not None:
            cmd["cpus"] = cpus
        if memory_bytes is not None:
            cmd["memory_bytes"] = memory_bytes
        return self._send(cmd)

    def vm_shutdown(self, name: str) -> dict:
        """Gracefully shut down and deregister a VM."""
        return self._send({"command": "shutdown", "name": name})

    def vm_kill(self, name: str) -> dict:
        """Force-kill and deregister a VM."""
        return self._send({"command": "kill", "name": name})

    def vm_destroy(self, name: str) -> dict:
        """Stop and deregister a VM."""
        return self._send({"command": "destroy", "name": name})


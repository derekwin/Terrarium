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


_POLICY_KEYS = {"read_paths", "write_paths", "net_allow", "memory_mb", "procs"}


def validate_policy(policy: dict) -> dict:
    """Light client-side check of an exec-policy dict (the engine enforces too).

    Raises ValueError on unknown keys, non-absolute path grants, or an
    empty ``net_allow`` list. Returns a plain copy of *policy*.
    """
    unknown = set(policy) - _POLICY_KEYS
    if unknown:
        raise ValueError(
            f"unknown policy keys: {sorted(unknown)} (known: {sorted(_POLICY_KEYS)})"
        )
    for key in ("read_paths", "write_paths"):
        for p in policy.get(key) or []:
            if not str(p).startswith("/"):
                raise ValueError(f"policy {key} entries must be absolute paths: {p!r}")
    if "net_allow" in policy and policy["net_allow"] is not None:
        na = policy["net_allow"]
        if not isinstance(na, list) or not na:
            raise ValueError(
                "policy net_allow must be a non-empty list "
                "(omit it for unrestricted network)"
            )
    return dict(policy)


class TerraClient:
    """Client for the terrarium engine daemon.

    Addresses:
    - unix socket path (default, local daemon)
    - "tcp://host:port" (remote daemon started with `--tcp`; pass
      `token=` when the server sets TERRA_TOKEN)
    """

    def __init__(self, socket_path: str | None = None, token: str | None = None):
        if socket_path is None:
            import os

            socket_path = os.environ.get("TERRA_SOCKET")
            if not socket_path:
                from . import paths

                socket_path = paths.default_socket()
        self.socket_path = socket_path
        self.token = token

    def _connect(self) -> socket.socket:
        if self.socket_path.startswith("tcp://"):
            host, port = self.socket_path[6:].rsplit(":", 1)
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.connect((host, int(port)))
            return sock
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(self.socket_path)
        return sock

    def _send(self, cmd: dict) -> dict:
        """Send a JSON command and return the parsed response data."""
        sock = self._connect()
        sock.settimeout(30)
        try:
            if self.token:
                sock.sendall((self.token + "\n").encode())
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
        system: str | None = None,
        upper: str | None = None,
        net: bool = False,
    ) -> dict:
        """Create a new VM.

        Args:
            layers: virtiofs layer names, highest priority first, base
                layer last. None = plain initramfs boot.
            upper: persistent upperdir name — user data survives VM
                destruction and is reused by later VMs with the same name.
            net: attach virtio-net (tap + host NAT; guest uses DHCP).
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
        if system:
            cmd["system"] = system
        if upper:
            cmd["upper"] = upper
        if net:
            cmd["net"] = True
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

    def vm_attach_fs(self, name: str, layers: list[str]) -> dict:
        """Hot-plug a layered filesystem into a running VM (warm pool)."""
        return self._send({"command": "attach_fs", "name": name, "layers": list(layers)})

    def pool_create(self, size: int, *, kernel: str | None = None, net: bool = False) -> dict:
        """Create warm-pool idle VMs."""
        cmd: dict = {"command": "pool_create", "pool_size": size}
        if kernel:
            cmd["kernel"] = kernel
        if net:
            cmd["net"] = True
        return self._send(cmd)

    def pool_list(self) -> dict:
        """List warm-pool slots and their claim state."""
        return self._send({"command": "pool_list"})

    def pool_claim(self, layers: list[str]) -> dict:
        """Claim an idle pool VM and hot-plug the given layers."""
        return self._send({"command": "pool_claim", "layers": list(layers)})

    def pool_release(self, name: str) -> dict:
        """Release a claimed pool VM back to idle."""
        return self._send({"command": "pool_release", "name": name})

    def vm_detach_fs(self, name: str) -> dict:
        """Detach a previously attached layered filesystem."""
        return self._send({"command": "detach_fs", "name": name})

    def vm_exec(
        self, name: str, args: list[str], timeout_secs: int = 60, *,
        sandbox: bool = False,
        policy: dict | None = None,
    ) -> dict:
        """Execute a command inside the VM via the guest agent (vsock).

        Retries transient agent-boot errors (handshake/vsock connect)
        for a few seconds — a VM is "Running" before its agent is ready.

        sandbox: run under sandlock permission isolation (the guest
        agent wraps argv with ``sandlock run <policy> --``; hard error
        when no sandlock binary exists in the guest).
        policy: optional exec-policy dict (see ``validate_policy``) —
        only meaningful with ``sandbox=True``; the engine rejects the
        combination otherwise.
        """
        import time as _time

        cmd: dict = {
            "command": "exec",
            "name": name,
            "args": list(args),
            "timeout_secs": timeout_secs,
        }
        if sandbox:
            cmd["sandbox"] = True
        if policy is not None:
            cmd["policy"] = validate_policy(policy)
        last: Exception | None = None
        for _ in range(16):
            try:
                return self._send(cmd)
            except TerraError as e:
                if "handshake" not in str(e) and "vsock" not in str(e):
                    raise
                last = e
                _time.sleep(0.5)
        raise last  # type: ignore[misc]

    def vm_destroy(self, name: str) -> dict:
        """Stop and deregister a VM."""
        return self._send({"command": "destroy", "name": name})

    # ── first-class sandboxes (S-M2) ────────────────────────────────

    def sandbox_create(self, tenant: str, policy: dict | None = None, *,
                       pool: bool = True, **vmspec) -> dict:
        """Create a sandbox on the tenant's shared VM.

        Idempotent: an existing tenant VM is reused and the VM-spec
        fields (kernel, initramfs, layers, cpus, memory_mb, net, ...)
        are ignored. ``policy`` is the stored exec policy (see
        ``validate_policy``); per-call ``sandbox_exec`` policies
        override it once without changing it.
        ``pool`` (default True): claim the tenant VM from the warm pool
        when idle slots are available; False forces a cold-booted
        dedicated ``tenant-<t>`` VM.
        Returns ``{id, vm, workdir, pool}`` — ``pool`` tells whether the
        tenant VM is warm-pool backed (vm name is then ``pool-N``).
        """
        cmd = {"command": "sandbox_create", "tenant": tenant}
        if not pool:
            cmd["pool"] = False
        cmd.update(vmspec)
        if policy is not None:
            cmd["policy"] = validate_policy(policy)
        return self._send(cmd)

    def sandbox_exec(
        self,
        id: str,
        args: list[str],
        timeout_secs: int | None = None,
        *,
        sandbox: bool | None = None,
        exec_mode: str | None = None,
        policy: dict | None = None,
    ) -> dict:
        """Execute args in a sandbox (cwd = its workdir, set engine-side).

        ``sandbox=None`` → engine default (True — confined via sandlock).
        ``exec_mode="background"`` → returns ``{session_id, ...}``.
        ``policy`` is a per-call override of the sandbox's stored
        policy (the stored policy is unaffected).
        """
        cmd: dict = {"command": "sandbox_exec", "id": id, "args": list(args)}
        if timeout_secs is not None:
            cmd["timeout_secs"] = timeout_secs
        if sandbox is not None:
            cmd["sandbox"] = bool(sandbox)
        if exec_mode is not None:
            cmd["exec_mode"] = exec_mode
        if policy is not None:
            cmd["policy"] = validate_policy(policy)
        return self._send(cmd)

    def sandbox_list(self, tenant: str | None = None) -> dict:
        """List sandbox records, optionally filtered by tenant."""
        cmd: dict = {"command": "sandbox_list"}
        if tenant:
            cmd["tenant"] = tenant
        return self._send(cmd)

    def sandbox_info(self, id: str) -> dict:
        """Get one sandbox record."""
        return self._send({"command": "sandbox_info", "id": id})

    def sandbox_kill(self, id: str) -> dict:
        """Kill a sandbox's sessions, remove its workdir, drop the record.

        The shared tenant VM keeps running.
        """
        return self._send({"command": "sandbox_kill", "id": id})

    def tenant_destroy(self, tenant: str) -> dict:
        """Destroy the tenant VM and all its sandbox records.

        Accepts the bare tenant id or the full VM name (``tenant-<id>``).
        """
        tenant = tenant.removeprefix("tenant-")
        return self._send({"command": "tenant_destroy", "tenant": tenant})


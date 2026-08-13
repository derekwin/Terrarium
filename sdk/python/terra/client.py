"""Low-level JSON client for the Terrarium engine daemon.

Communicates over a Unix domain socket with newline-delimited JSON.
"""

from __future__ import annotations

import json
import socket

from .exceptions import TerraError


_POLICY_KEYS = {"capabilities", "limits", "default", "audit", "version"}
_CAPABILITY_TYPES = {"File", "Network", "Device"}
_FILE_ACCESS = {"Read", "ReadWrite"}
_DIRECTIONS = {"Outbound"}
_LIMIT_KEYS = {"memory_mb", "procs", "fds", "cpu_shares"}


def _validate_file_capability(spec: dict) -> None:
    """Validate a File capability spec (path pattern + access)."""
    path = spec.get("path")
    if not isinstance(path, dict) or len(path) != 1:
        raise ValueError(
            f"File capability path must be a single-key "
            f"{{'Prefix'|'Exact': path}} dict, got {path!r}"
        )
    (pattern, p), = path.items()
    if pattern not in {"Prefix", "Exact"}:
        raise ValueError(
            f"File path pattern must be 'Prefix' or 'Exact', got {pattern!r}"
        )
    if not isinstance(p, str) or not p.startswith("/"):
        raise ValueError(f"File {pattern} path must be absolute: {p!r}")
    access = spec.get("access")
    if access not in _FILE_ACCESS:
        raise ValueError(
            f"File access must be one of {sorted(_FILE_ACCESS)}, got {access!r}"
        )


def _validate_network_capability(spec: dict) -> None:
    """Validate a Network capability spec (endpoint + direction)."""
    endpoint = spec.get("endpoint")
    if not isinstance(endpoint, dict):
        raise ValueError(
            f"Network capability endpoint must be a dict, got {endpoint!r}"
        )
    host = endpoint.get("host")
    if not isinstance(host, str) or not host:
        raise ValueError(
            f"Network endpoint host must be a non-empty string, got {host!r}"
        )
    port = endpoint.get("port")
    if port is not None and (
        not isinstance(port, int) or isinstance(port, bool) or port <= 0 or port > 65535
    ):
        raise ValueError(
            f"Network endpoint port must be a positive int in 1..65535, got {port!r}"
        )
    direction = spec.get("direction")
    if direction not in _DIRECTIONS:
        raise ValueError(
            f"Network direction must be one of {sorted(_DIRECTIONS)}, got {direction!r}"
        )


def validate_policy(policy: dict) -> dict:
    """Client-side validation of a SandboxPolicy dict (the engine enforces too).

    The policy is the new JSON shape the engine consumes::

        {
            "capabilities": [
                {"File": {"path": {"Prefix": "/opt"}, "access": "Read"}},
                {"Network": {"endpoint": {"host": "api.openai.com", "port": 443},
                             "direction": "Outbound"}},
            ],
            "limits": {"memory_mb": 512, "procs": 20},
            "version": 1,          # optional; default 0
            "default": "deny",     # optional; default deny
            "audit": {"deny": True, "exec": False, "resource": False},  # optional
        }

    Checks:
    - unknown top-level keys are rejected
    - ``capabilities`` is a list of single-key dicts whose key is
      ``File`` / ``Network`` / ``Device``
    - File: path is ``{"Prefix": p}`` or ``{"Exact": p}`` with an
      absolute *p*; access in ``{Read, ReadWrite, Execute}``
    - Network: endpoint ``{"host": non-empty str, "port": optional int
      in 1..65535}``; direction in ``{Outbound, Inbound}``
    - Device: absolute path
    - ``limits`` (optional dict): keys in ``{memory_mb, procs, fds,
      bandwidth_kbps, cpu_shares}``, values positive ints
    - ``default`` in ``{"deny", "allow"}`` — ``"allow"`` raises (D6:
      the escape hatch is not exposed through the SDK)
    - ``version`` is an int when present

    Raises ValueError on any violation. Returns a deep copy of the
    normalized policy, passed to the wire as-is.
    """
    import copy

    unknown = set(policy) - _POLICY_KEYS
    if unknown:
        raise ValueError(
            f"unknown policy keys: {sorted(unknown)} (known: {sorted(_POLICY_KEYS)})"
        )

    normalized = copy.deepcopy(policy)

    # capabilities: list of single-key {"File"|"Network"|"Device": spec}
    # Always present in the normalized output (empty list = default-deny
    # base; the engine unions it with the default capability set).
    caps = normalized.get("capabilities", [])
    if caps is None:
        caps = []
    if not isinstance(caps, list):
        raise ValueError(
            f"policy capabilities must be a list, got {type(caps).__name__}"
        )
    normalized["capabilities"] = caps
    for i, cap in enumerate(caps):
        if not isinstance(cap, dict) or len(cap) != 1:
            raise ValueError(
                f"capability #{i} must be a single-key dict, got {cap!r}"
            )
        (kind, spec), = cap.items()
        if kind not in _CAPABILITY_TYPES:
            raise ValueError(
                f"unknown capability type {kind!r} "
                f"(known: {sorted(_CAPABILITY_TYPES)})"
            )
        if not isinstance(spec, dict):
            raise ValueError(
                f"capability {kind!r} spec must be a dict, got {spec!r}"
            )
        if kind == "File":
            _validate_file_capability(spec)
        elif kind == "Network":
            _validate_network_capability(spec)
        else:  # Device
            raise ValueError(
                "Device capability is not supported by the sandlock backend"
            )

    # limits: optional dict of positive ints
    limits = normalized.get("limits")
    if limits is not None:
        if not isinstance(limits, dict):
            raise ValueError(
                f"policy limits must be a dict, got {type(limits).__name__}"
            )
        unknown_limits = set(limits) - _LIMIT_KEYS
        if unknown_limits:
            raise ValueError(
                f"unknown limit keys: {sorted(unknown_limits)} "
                f"(known: {sorted(_LIMIT_KEYS)})"
            )
        for key, value in limits.items():
            if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
                raise ValueError(f"limit {key} must be a positive int, got {value!r}")

    # default: deny-by-default only through the SDK (D6 — the "allow"
    # escape hatch is not exposed)
    default = normalized.get("default")
    if default is not None:
        if default not in {"deny", "allow"}:
            raise ValueError(
                f"policy default must be 'deny' or 'allow', got {default!r}"
            )
        if default == "allow":
            raise ValueError(
                "policy default 'allow' is not permitted — the escape hatch "
                "is unavailable through the SDK (default is deny)"
            )

    # version: optional int
    if "version" in normalized and (
        not isinstance(normalized["version"], int)
        or isinstance(normalized["version"], bool)
    ):
        raise ValueError(
            f"policy version must be an int, got {normalized['version']!r}"
        )

    return normalized


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
        # The daemon writes the response only when the command finishes,
        # and an exec can legitimately run for minutes (timeout_secs up to
        # 3600). Scale the socket read timeout to the command's declared
        # budget plus headroom instead of a fixed 30s that kills long
        # execs mid-flight.
        # VM lifecycle commands (sandbox_create etc.) include guest boot
        # + agent readiness, which can exceed 30s on large layers — give
        # commands without an explicit budget a generous default.
        declared = cmd.get("timeout_secs")
        sock_timeout = 180 if not declared else min(int(declared) + 30, 3700)
        sock.settimeout(sock_timeout)
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

    def pool_shrink(self, count: int) -> dict:
        """Atomically destroy *count* idle pool VMs (engine-side).

        Claimed slots are never touched; runs under the engine's manager
        lock, closing the client-side scale TOCTOU window. Returns
        ``{"removed": [names], "count": N}``.
        """
        return self._send({"command": "pool_shrink", "pool_size": count})

    def vm_detach_fs(self, name: str) -> dict:
        """Detach a previously attached layered filesystem."""
        return self._send({"command": "detach_fs", "name": name})

    def vm_snapshot(self, name: str, snapshot_path: str | None = None) -> dict:
        """Capture a VM at its current state (P1 fast-reset primitive).

        Default path is ``{snapshot_dir}/terra-snap-<vm>`` — a DIRECTORY
        that CH fills with the memory + state files. Restore later with
        :meth:`vm_restore`.
        """
        cmd: dict = {"command": "snapshot", "name": name}
        if snapshot_path:
            cmd["snapshot_path"] = snapshot_path
        return self._send(cmd)

    def vm_restore(
        self,
        name: str,
        snapshot_path: str,
        *,
        cpus: int | None = None,
        memory_mb: int | None = None,
        layers: list[str] | None = None,
        kernel: str | None = None,
        net: bool = False,
    ) -> dict:
        """Create a NEW VM whose guest state comes from a snapshot.

        ``layers`` re-composes the host-side rootfs stack for the restored
        VM. The guest's cpus/memory config comes from the snapshot itself
        (``config.json``) — CH restore is a restore-only invocation.
        """
        cmd: dict = {
            "command": "restore",
            "name": name,
            "snapshot_path": snapshot_path,
            "net": bool(net),
        }
        if cpus is not None:
            cmd["cpus"] = cpus
        if memory_mb is not None:
            cmd["memory_mb"] = memory_mb
        if layers:
            cmd["layers"] = list(layers)
        if kernel:
            cmd["kernel"] = kernel
        return self._send(cmd)

    def vm_reset(self, name: str) -> dict:
        """In-place episode reset (P1/RL fast path).

        The VM keeps running: the guest kills its episode processes and
        clears the episode-writable runtime dirs (/workdir, /tmp, /run)
        back to the LAYER baseline. Far cheaper than destroy + restore.
        """
        return self._send({"command": "reset_vm", "name": name})

    def audit_list(
        self,
        *,
        limit: int | None = None,
        event: str | None = None,
        sandbox_id: str | None = None,
        history: bool = False,
    ) -> dict:
        """Query audit events (P2 observability).

        Returns ``{audit: [{ts_ms, event, sandbox_id, args, exit_code,
        duration_ms, reason, kind, detail}], count}`` — newest first.
        ``event`` filters to ``"exec" | "deny" | "resource"``.
        ``history=True`` reads the persisted JSONL trail (survives daemon
        restarts) instead of the in-memory ring buffer.
        """
        cmd: dict = {"command": "audit_list"}
        if limit is not None:
            cmd["limit"] = limit
        if event is not None:
            cmd["event"] = event
        if sandbox_id is not None:
            cmd["id"] = sandbox_id
        if history:
            cmd["audit_history"] = True
        return self._send(cmd)

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

    # ── background exec sessions (engine-tracked) ─────────────────

    def session_status(self, session_id: str) -> dict:
        """Query a background exec session's status.

        Returns ``{session_id, vm_name, args, status, exit_code, stdout,
        stderr, sandbox}`` (see ``docs/protocol.md``).
        """
        return self._send({"command": "session_status", "session_id": session_id})

    def session_kill(self, session_id: str) -> dict:
        """Kill a background exec session (killpg in the guest).

        Returns ``{session_id, status: "killed"}``. Hard error for an
        unknown or non-running session, or a gone VM.
        """
        return self._send({"command": "session_kill", "session_id": session_id})

    def session_list(self) -> dict:
        """List all background exec sessions.

        Returns ``{sessions: [{session_id, vm_name, status, sandbox}],
        count}``.
        """
        return self._send({"command": "session_list"})

    def tenant_destroy(self, tenant: str) -> dict:
        """Destroy the tenant VM and all its sandbox records.

        Accepts the bare tenant id or the full VM name (``tenant-<id>``).
        """
        tenant = tenant.removeprefix("tenant-")
        return self._send({"command": "tenant_destroy", "tenant": tenant})

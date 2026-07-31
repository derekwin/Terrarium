"""Sandbox — a session inside a tenant VM, sharing the VM with other sessions.

VM = tenant isolation boundary, Sandbox = agent session inside a VM.
Multiple Sandboxes share one VM per tenant. If a tenant has no VM,
the first Sandbox creates it.

Usage::

    from terra.sandbox import Sandbox

    # First sandbox for a tenant — creates the VM:
    sb1 = Sandbox(tenant="my-org", template="py312")

    # Second sandbox — reuses the same VM, new session:
    sb2 = Sandbox(tenant="my-org")
    sb2.exec("python3 -c 'print(2+2)'")

    # Both share the same VM, independent workdirs:
    sb1.id  # "sb-a3f2b1c4" (engine-allocated)
    sb2.id  # "sb-b1c4d5e6"

    # Context manager — auto-kills session on exit:
    with Sandbox(template="py312") as sb:
        print(sb.exec(["python3", "--version"]).stdout)

    # Destroy the whole tenant (all sandboxes):
    Sandbox.destroy_tenant("my-org")
"""

from __future__ import annotations

import base64
import shlex
from dataclasses import dataclass
from uuid import uuid4


from ._engine import DaemonManager
from .client import TerraClient, TerraError as ClientError, validate_policy
from . import images
from .template import Template
from .exceptions import (
    ExecResult,
    ExecError,
    TerraError,
    SandboxTimeoutError,
    SandboxStateError,
)

# Template base label → engine system layer name.
_SYSTEM_MAP: dict[str, str] = {"alpine": "base", "ubuntu": "ubuntu"}


@dataclass
class FileInfo:
    """Metadata for a file or directory inside a sandbox."""

    name: str
    size: int = 0
    is_dir: bool = False
    mtime: str = ""


class FilesClient:
    """File operations inside a sandbox, bridged via :meth:`Sandbox.exec`.

    All paths are resolved relative to the guest's working directory
    (``/workdir/<session>`` by default).  Use absolute paths when needed.
    """

    def __init__(self, sandbox: Sandbox):
        self._sb = sandbox

    def read(self, path: str) -> str:
        """Read a file from the sandbox and return its content as a string."""
        result = self._sb.exec(["cat", path])
        return result.stdout

    def write(self, path: str, content: str) -> None:
        """Write *content* into a file inside the sandbox.

        The content is base64-encoded so that shell metacharacters are
        handled safely and binary payloads can be written.
        """
        encoded = base64.b64encode(content.encode()).decode()
        self._sb.exec(["sh", "-c", f"echo {encoded} | base64 -d > {path}"])

    def upload(self, local_path: str, remote_path: str) -> None:
        """Upload a file from the host to the sandbox."""
        with open(local_path, "rb") as f:
            data = base64.b64encode(f.read()).decode()
        self._sb.exec(["sh", "-c", f"echo {data} | base64 -d > {remote_path}"])

    def download(self, remote_path: str, local_path: str) -> None:
        """Download a file from the sandbox to the host."""
        result = self._sb.exec(["cat", remote_path])
        with open(local_path, "w") as f:
            f.write(result.stdout)

    def list(self, path: str) -> list[FileInfo]:
        """List files and directories at *path* inside the sandbox."""
        result = self._sb.exec(["ls", "-la", path])
        lines = result.stdout.strip().split("\n")
        # The first line is the "total N" summary — skip it.
        files: list[FileInfo] = []
        for line in lines[1:]:
            parts = line.split()
            if len(parts) >= 9:
                files.append(
                    FileInfo(
                        name=parts[8],
                        is_dir=line.startswith("d"),
                        size=int(parts[4]) if parts[4].isdigit() else 0,
                    )
                )
        return files

    def mkdir(self, path: str) -> None:
        """Create a directory (and parents) inside the sandbox."""
        self._sb.exec(["mkdir", "-p", path])

    def remove(self, path: str) -> None:
        """Remove a file or directory tree inside the sandbox."""
        self._sb.exec(["rm", "-rf", path])

    def exists(self, path: str) -> bool:
        """Return *True* if *path* exists inside the sandbox."""
        result = self._sb.exec(["test", "-e", path], check=False)
        return result.exit_code == 0


class Sandbox:
    """A session inside a shared tenant VM.

    Multiple Sandbox instances for the same tenant share a single VM
    but have independent working directories and exec contexts.

    Parameters
    ----------
    tenant:
        Tenant identifier. Auto-generated if *None*.
        Sandboxes with the same tenant share one VM.
    template:
        Named template to load (resolves ``layers``, ``kernel``).
        Required for the first sandbox in a tenant.
        Mutually replaces explicit ``layers`` and ``kernel``.
    layers:
        Explicit virtiofs layer names, highest priority first, base last.
        Defaults to ``["base"]`` when neither *template* nor *layers* given.
    kernel:
        Kernel variant name or path.  Default kernel when omitted.
    backend:
        Reserved for future multi-backend selection (``"auto"`` currently).
    cpu:
        Number of vCPUs (default 1).
    memory_mb:
        Guest RAM in MiB (default 256).
    network:
        Truthy → attach virtio-net (NAT + DHCP).
    policy:
        Optional exec-policy dict (plain dict, validated client-side):
        ``read_paths``/``write_paths`` append path grants to the default
        policy (RO system dirs, RW session workdir + ``/tmp``, network
        unrestricted), ``net_allow`` (non-empty list) switches egress to
        deny-by-default, ``memory_mb``/``procs`` set resource limits.
        Stored on the sandbox engine-side; echoed by ``sandbox_info``.
    env:
        Default environment variables for :meth:`exec` (stored as metadata;
        not yet wired to guest agent).
    timeout:
        Default per-command timeout in seconds (used when :meth:`exec` does
        not specify its own *timeout*).
    metadata:
        Arbitrary user metadata dictionary.
    pool:
        When the tenant has no VM yet, claim an idle warm-pool VM
        (default True — millisecond hot start).  False forces a
        cold-booted dedicated ``tenant-<t>`` VM.  The tenant VM is
        ``pool-N`` when pool-backed; ``tenant_destroy`` releases it
        back to the pool instead of destroying it.
    """

    # ── construction ──────────────────────────────────────────────

    def __init__(
        self,
        tenant: str | None = None,
        *,
        template: str | None = None,
        layers: list[str] | None = None,
        kernel: str | None = None,
        backend: str = "auto",
        cpu: int | None = None,
        memory_mb: int | None = None,
        network: bool = False,
        policy: dict | None = None,
        env: dict[str, str] | None = None,
        timeout: int = 600,
        metadata: dict | None = None,
        pool: bool = True,
    ):
        # -- identity ---------------------------------------------------------
        self._tenant: str = tenant or uuid4().hex[:8]
        self._id: str | None = None
        self._vm_name: str = f"tenant-{self._tenant}"
        self._policy: dict = validate_policy(policy) if policy else {}

        # -- auto-start daemon ------------------------------------------------
        dm = DaemonManager()
        dm.ensure_running()

        # -- resolve the VM spec (only needed when the tenant VM is new) ------
        # The tenant may already own a VM — either a dedicated
        # ``tenant-<t>`` (cold boot) or a claimed warm-pool VM (``pool-N``).
        # The engine indexes both by tenant (sandbox record → VM), so probe
        # the sandbox registry instead of guessing the VM name.
        client = TerraClient()
        try:
            existing = client.sandbox_list(self._tenant).get("sandboxes", [])
        except ClientError:
            existing = []
        vm_exists = bool(existing)

        vmspec: dict = {}
        if not vm_exists:
            if not template and not layers:
                raise TerraError(
                    "template or layers required for first sandbox in tenant",
                    sandbox_id=self._vm_name,
                )

            # -- resolve template ----------------------------------------------
            system_layer: str | None = None
            if template:
                t = Template.load(template)
                system_layer = _SYSTEM_MAP.get(t.base, t.base)
                if layers is None:
                    layers = list(t.layers)
                    # overlayfs rejects duplicate lower layers (ELOOP).
                    if system_layer not in layers:
                        layers.append(system_layer)
                if kernel is None and t.kernel:
                    kernel = t.kernel

            if layers is None:
                layers = ["base"]

            # -- resolve kernel ------------------------------------------------
            if kernel:
                from pathlib import Path as _Path

                if _Path(kernel).exists():
                    kernel_path = str(_Path(kernel).expanduser())
                else:
                    kernel_path = str(images.resolve_kernel(kernel))
            else:
                kernel_path = str(images.ensure("vmlinux.bin"))

            vmspec = {
                "kernel": kernel_path,
                "initramfs": str(images.resolve_rootfs("virtiofs")),
                "layers": list(layers),
                "cpus": cpu or 1,
                # CPU headroom for online resize (mirrors the old
                # client.vm_create default; memory stays fixed-size).
                "max_cpus": 16,
                "memory_mb": memory_mb or 256,
                "net": bool(network),
            }

        # -- one engine call: ensure VM + allocate id + mkdir workdir ---------
        # (retry transient agent-boot races on a freshly spawned VM)
        import time as _time

        last: Exception | None = None
        for _ in range(20):
            try:
                resp = client.sandbox_create(
                    self._tenant, policy=self._policy or None, pool=pool, **vmspec
                )
                break
            except ClientError as e:
                msg = str(e)
                if "handshake" not in msg and "vsock" not in msg:
                    raise
                last = e
                _time.sleep(0.5)
        else:
            raise last  # type: ignore[misc]

        self._id = resp["id"]
        self._vm_name = resp["vm"]
        self._workdir: str = resp["workdir"]
        self._client = client
        self._alive: bool = True
        self._default_timeout = timeout
        self._backend: str = "ch"  # default; can be detected from info later
        self._env: dict[str, str] = dict(env or {})
        self._from_pool: bool = False  # legacy Pool.acquire() sessions only
        self._pool_backed: bool = bool(resp.get("pool", False))
        self.metadata: dict = metadata or {}

    # ── properties ─────────────────────────────────────────────────

    @property
    def id(self) -> str:
        """Engine-allocated sandbox identifier (``sb-<8hex>``).

        Pool-claimed sessions (not engine sandboxes) fall back to the
        ``{vm}/{session}`` composite.
        """
        if self._id is not None:
            return self._id
        return f"{self._vm_name}/{self._session_id}"

    @property
    def vm(self) -> str:
        """The tenant VM name this sandbox runs in."""
        return self._vm_name

    @property
    def pool_backed(self) -> bool:
        """True when the tenant VM was claimed from the warm pool."""
        return self._pool_backed

    @property
    def tenant(self) -> str:
        """The tenant identifier (VM name is ``tenant-<tenant>``)."""
        return self._tenant

    @property
    def policy(self) -> dict:
        """The stored exec policy, as echoed by the engine.

        Pool-claimed sessions (not engine sandboxes) report the local
        copy — always empty today.
        """
        if self._id is not None and not self._from_pool:
            try:
                return self._client.sandbox_info(self._id).get("policy") or {}
            except ClientError:
                pass
        return dict(self._policy)

    @property
    def status(self) -> str:
        """Current sandbox status: ``"running"``, ``"paused"``, ``"stopped"``, or ``"unknown"``."""
        if not self._alive:
            return "stopped"
        try:
            info = self._client.vm_info(self._vm_name)
        except ClientError:
            return "stopped"
        state: str = info.get("state", "")
        return {
            "Running": "running",
            "Paused": "paused",
            "ShutDown": "stopped",
        }.get(state, "unknown")

    @property
    def backend(self) -> str:
        """Backend in use (``"ch"`` for Cloud Hypervisor)."""
        return self._backend

    @property
    def env(self) -> dict[str, str]:
        """Default environment variables for :meth:`exec`."""
        return self._env

    @env.setter
    def env(self, value: dict[str, str]) -> None:
        self._env = dict(value)

    @property
    def files(self) -> FilesClient:
        """File operations client (read, write, upload, download, list, etc.).

        Lazily created on first access.
        """
        if not hasattr(self, "_files"):
            self._files = FilesClient(self)
        return self._files

    # ── exec ───────────────────────────────────────────────────────

    def exec(
        self,
        command: str | list[str],
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        timeout: int | None = None,
        check: bool = False,
        sandboxed: bool = True,
        policy: dict | None = None,
    ) -> ExecResult:
        """Execute a command inside the sandbox.

        Parameters
        ----------
        command:
            Command as a string (split shell-style with ``shlex``) or
            list of args.
        cwd:
            Working directory inside the guest. Defaults to the sandbox's
            private workdir (set engine-side); only pass this to override.
        env:
            Extra environment variables prepended to the command
            (merged with sandbox-level *env*).
        timeout:
            Per-command timeout in seconds (defaults to sandbox-level *timeout*).
        check:
            If *True*, raise :class:`~terra.exceptions.ExecError` on non-zero
            exit code.
        sandboxed:
            Run under sandlock permission isolation (default *True* —
            isolation is the product point). The default policy makes
            the system read-only; only the session workdir and ``/tmp``
            are writable, and the network is unrestricted for now.
        policy:
            Per-call exec-policy override (same dict shape as the
            *policy* constructor arg). Wins for this call only; the
            stored policy is unaffected.

        Returns
        -------
        ExecResult
            Structured result with ``exit_code``, ``stdout``, ``stderr``.

        Raises
        ------
        ExecError
            When *check=True* and the command exits non-zero.
        SandboxStateError
            When the sandbox is no longer alive.
        SandboxTimeoutError
            When the engine times out executing the command.
        """
        if not self._alive:
            raise SandboxStateError(
                f"Sandbox {self.id} is killed",
                sandbox_id=self.id,
            )

        # Normalise to list-of-strings (shlex so quoting survives).
        if isinstance(command, str):
            args = shlex.split(command)
        else:
            args = list(command)

        # Combine sandbox-level env + command-level env.
        full_env: dict[str, str] = dict(self._env)
        if env:
            full_env.update(env)

        timeout_secs = timeout if timeout is not None else self._default_timeout
        call_policy = validate_policy(policy) if policy is not None else None

        if self._from_pool:
            # Pool-claimed VMs are not engine sandboxes: exec directly on
            # the VM, cd-ing into the session workdir (legacy path).
            effective_cwd: str = cwd if cwd is not None else self._workdir
            quoted = shlex.join(args)
            if full_env:
                prefix = " ".join(
                    f"{k}={shlex.quote(str(v))}" for k, v in full_env.items()
                )
                args = ["sh", "-c", f"cd {shlex.quote(effective_cwd)} && {prefix} {quoted}"]
            else:
                args = ["sh", "-c", f"cd {shlex.quote(effective_cwd)} && {quoted}"]
            try:
                resp = self._client.vm_exec(
                    self._vm_name, args, timeout_secs, sandbox=sandboxed,
                    policy=call_policy,
                )
            except ClientError as e:
                msg = str(e)
                if "timeout" in msg.lower():
                    raise SandboxTimeoutError(
                        msg, sandbox_id=self.id, engine_error=msg
                    ) from e
                raise TerraError(
                    msg, sandbox_id=self.id, engine_error=msg
                ) from e
        else:
            # Engine sandbox: cwd defaults to the sandbox workdir
            # engine-side — only wrap when cwd/env override is needed.
            if cwd is not None or full_env:
                quoted = shlex.join(args)
                if full_env:
                    prefix = " ".join(
                        f"{k}={shlex.quote(str(v))}" for k, v in full_env.items()
                    )
                    quoted = f"{prefix} {quoted}"
                if cwd is not None:
                    quoted = f"cd {shlex.quote(cwd)} && {quoted}"
                args = ["sh", "-c", quoted]
            try:
                resp = self._client.sandbox_exec(
                    self._id, args, timeout_secs, sandbox=sandboxed,
                    policy=call_policy,
                )
            except ClientError as e:
                msg = str(e)
                if "timeout" in msg.lower():
                    raise SandboxTimeoutError(
                        msg, sandbox_id=self.id, engine_error=msg
                    ) from e
                raise TerraError(
                    msg, sandbox_id=self.id, engine_error=msg
                ) from e

        result = ExecResult(
            exit_code=resp.get("exit_code", -1),
            stdout=resp.get("stdout", ""),
            stderr=resp.get("stderr", ""),
        )

        if check and result.exit_code != 0:
            raise ExecError(
                f"Command exited with {result.exit_code}",
                exec_result=result,
                sandbox_id=self.id,
            )

        return result

    # ── lifecycle ──────────────────────────────────────────────────

    def kill(self) -> None:
        """Kill this sandbox session (does NOT destroy the shared tenant VM).

        Engine-side: kills the sandbox's running sessions, removes the
        workdir and drops the record. Other sandboxes on the same tenant
        VM continue running.

        Idempotent — safe to call multiple times (a second kill gets
        "not found" from the engine and is a no-op).
        """
        if not self._alive:
            return
        try:
            if self._from_pool:
                self._client.vm_exec(
                    self._vm_name, ["rm", "-rf", self._workdir], timeout_secs=5
                )
            else:
                self._client.sandbox_kill(self._id)
        except ClientError:
            pass
        self._alive = False

    @classmethod
    def destroy_tenant(cls, tenant_id: str) -> None:
        """Destroy the tenant VM and all its sandboxes.

        Parameters
        ----------
        tenant_id:
            The tenant identifier (the VM name is ``tenant-<tenant_id>``).
        """
        client = TerraClient()
        client.tenant_destroy(tenant_id)

    def metrics(self) -> dict:
        """Query current resource usage.

        Returns a dict with keys ``cpu_count`` and ``memory_mb``.
        """
        if not self._alive:
            raise SandboxStateError(
                f"Sandbox {self.id} is killed",
                sandbox_id=self.id,
            )
        info = self._client.vm_info(self._vm_name)
        return {
            "cpu_count": info.get("cpus"),
            "memory_mb": info.get("memory_mb"),
        }

    def resize(self, cpu: int | None = None, memory_mb: int | None = None) -> None:
        """Resize the tenant VM's resources online (no reboot required).

        Parameters
        ----------
        cpu:
            New vCPU count.
        memory_mb:
            New memory in MiB.
        """
        if not self._alive:
            raise SandboxStateError(
                f"Sandbox {self.id} is killed",
                sandbox_id=self.id,
            )
        kwargs: dict = {}
        if cpu is not None:
            kwargs["cpus"] = cpu
        if memory_mb is not None:
            kwargs["memory_bytes"] = memory_mb * 1024 * 1024
        self._client.vm_resize(self._vm_name, **kwargs)

    # ── context manager ────────────────────────────────────────────

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, *args: object) -> None:
        self.kill()

    def __repr__(self) -> str:
        status = self.status if self._alive else "stopped"
        return f"Sandbox(id={self.id!r}, vm={self._vm_name!r}, status={status!r})"

    def __del__(self) -> None:
        """Best-effort cleanup on GC."""
        if getattr(self, "_alive", False):
            try:
                self.kill()
            except Exception:
                pass

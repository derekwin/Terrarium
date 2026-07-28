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
    sb1.id  # "tenant-my-org/sb-a3f2"
    sb2.id  # "tenant-my-org/sb-b1c4"

    # Context manager — auto-kills session on exit:
    with Sandbox(template="py312") as sb:
        print(sb.exec(["python3", "--version"]).stdout)

    # Destroy the whole tenant (all sandboxes):
    Sandbox.destroy_tenant("my-org")
"""

from __future__ import annotations

import base64
from dataclasses import dataclass
from uuid import uuid4


from ._engine import DaemonManager
from .client import TerraClient, TerraError as ClientError
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
    env:
        Default environment variables for :meth:`exec` (stored as metadata;
        not yet wired to guest agent).
    timeout:
        Default per-command timeout in seconds (used when :meth:`exec` does
        not specify its own *timeout*).
    metadata:
        Arbitrary user metadata dictionary.
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
        env: dict[str, str] | None = None,
        timeout: int = 600,
        metadata: dict | None = None,
    ):
        # -- identity ---------------------------------------------------------
        self._tenant: str = tenant or f"tenant-{uuid4().hex[:8]}"
        self._session_id: str = f"sb-{uuid4().hex[:4]}"
        self._vm_name: str = f"tenant-{self._tenant}"

        # -- auto-start daemon ------------------------------------------------
        dm = DaemonManager()
        dm.ensure_running()

        # -- check if tenant VM already exists ---------------------------------
        client = TerraClient()
        vm_exists: bool = False
        try:
            info = client.vm_info(self._vm_name)
            vm_exists = True
        except ClientError:
            vm_exists = False

        if not vm_exists:
            # First sandbox for this tenant → create VM.
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
                    layers = t.layers + [system_layer]
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

            # -- create VM -----------------------------------------------------
            initramfs = str(images.resolve_rootfs("virtiofs"))
            client.vm_create(
                self._vm_name,
                kernel_path,
                initramfs=initramfs,
                layers=layers,
                cpus=cpu or 1,
                memory_mb=memory_mb or 256,
                net=bool(network),
            )
        else:
            # VM exists → validate it's running.
            if info.get("state") not in ("Running",):
                raise SandboxStateError(
                    f"Tenant VM {self._vm_name} is not running",
                    sandbox_id=self._vm_name,
                )

        # -- create per-sandbox workdir ----------------------------------------
        self._workdir: str = f"/workdir/{self._session_id}"
        self._client = client
        self._alive: bool = True
        self._default_timeout = timeout
        self._backend: str = "ch"  # default; can be detected from info later
        self._env: dict[str, str] = dict(env or {})
        self._from_pool: bool = False
        self.metadata: dict = metadata or {}

    # ── properties ─────────────────────────────────────────────────

    @property
    def id(self) -> str:
        """Full sandbox identifier: ``{vm_name}/{session_id}``."""
        return f"{self._vm_name}/{self._session_id}"

    @property
    def vm(self) -> str:
        """The tenant VM name this sandbox runs in."""
        return self._vm_name

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
    ) -> ExecResult:
        """Execute a command inside the sandbox.

        Parameters
        ----------
        command:
            Command as a string (will be ``split()``) or list of args.
        cwd:
            Working directory inside the guest (defaults to the sandbox's
            private workdir).
        env:
            Extra environment variables prepended to the command
            (merged with sandbox-level *env*).
        timeout:
            Per-command timeout in seconds (defaults to sandbox-level *timeout*).
        check:
            If *True*, raise :class:`~terra.exceptions.ExecError` on non-zero
            exit code.

        Returns
        -------
        ExecResult
            Structured result with ``exit_code``, ``stdout``, ``stderr``,
            ``duration_ms``, ``timed_out``.

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

        # Normalise to list-of-strings.
        if isinstance(command, str):
            args = command.split()
        else:
            args = list(command)

        # Set workdir to sandbox-specific directory unless overridden.
        effective_cwd: str = cwd if cwd is not None else self._workdir

        # Combine sandbox-level env + command-level env.
        full_env: dict[str, str] = dict(self._env)
        if env:
            full_env.update(env)

        if full_env:
            prefix = " ".join(f"{k}={v}" for k, v in full_env.items())
            args = ["sh", "-c", f"cd {effective_cwd} && {prefix} {' '.join(args)}"]
        else:
            args = ["sh", "-c", f"cd {effective_cwd} && {' '.join(args)}"]

        timeout_secs = timeout if timeout is not None else self._default_timeout

        try:
            resp = self._client.vm_exec(self._vm_name, args, timeout_secs)
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
            duration_ms=resp.get("duration_ms", 0),
            timed_out=resp.get("timed_out", False),
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

        Only removes the sandbox workdir. Other sandboxes on the same
        tenant VM continue running.

        Idempotent — safe to call multiple times.
        """
        if not self._alive:
            return
        try:
            self._client.vm_exec(
                self._vm_name, ["rm", "-rf", self._workdir], timeout_secs=5
            )
        except ClientError:
            pass
        self._alive = False

    @classmethod
    def destroy_tenant(cls, tenant_id: str) -> None:
        """Destroy the tenant VM and all its sandboxes.

        Parameters
        ----------
        tenant_id:
            The tenant identifier (the part after ``tenant-`` in the VM name).
        """
        client = TerraClient()
        vm_name = f"tenant-{tenant_id}"
        client.vm_destroy(vm_name)

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
        if self._alive:
            try:
                self.kill()
            except Exception:
                pass

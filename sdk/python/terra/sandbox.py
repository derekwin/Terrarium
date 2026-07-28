"""Sandbox — a unified API over VM creation, exec, lifecycle, and metrics.

Usage::

    from terra.sandbox import Sandbox

    sb = Sandbox(template="py312", network=True)
    result = sb.exec(["python3", "-c", "print(2+2)"], check=True)
    print(result.stdout)
    sb.kill()

    # Or as a context manager — auto-kills on exit:
    with Sandbox(template="py312") as sb:
        print(sb.exec(["python3", "--version"]).stdout)
"""

from __future__ import annotations

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


class Sandbox:
    """A running Terrarium sandbox (VM) with a unified high-level API.

    Parameters
    ----------
    template:
        Named template to load (resolves ``layers``, ``kernel``).
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
        template: str | None = None,
        *,
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
        # -- resolve template -------------------------------------------------
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

        # -- resolve kernel ---------------------------------------------------
        if kernel:
            from pathlib import Path as _Path

            if _Path(kernel).exists():
                kernel_path = str(_Path(kernel).expanduser())
            else:
                kernel_path = str(images.resolve_kernel(kernel))
        else:
            kernel_path = str(images.ensure("vmlinux.bin"))

        # -- resolve initramfs ------------------------------------------------
        initramfs = str(images.resolve_rootfs("virtiofs"))

        # -- auto-start daemon ------------------------------------------------
        dm = DaemonManager()
        dm.ensure_running()

        # -- create VM --------------------------------------------------------
        client = TerraClient()
        self._name: str = f"sandbox-{uuid4().hex[:8]}"

        client.vm_create(
            self._name,
            kernel_path,
            initramfs=initramfs,
            layers=layers,
            cpus=cpu or 1,
            memory_mb=memory_mb or 256,
            net=bool(network),
        )

        self._client = client
        self._alive: bool = True
        self._default_timeout = timeout
        self._backend: str = "ch"  # default; can be detected from info later
        self.metadata: dict = metadata or {}
        self.env: dict[str, str] = env or {}

    # ── properties ─────────────────────────────────────────────────

    @property
    def id(self) -> str:
        """Unique sandbox identifier (the VM name)."""
        return self._name

    @property
    def status(self) -> str:
        """Current sandbox status: ``"running"``, ``"paused"``, ``"stopped"``, or ``"unknown"``."""
        if not self._alive:
            return "stopped"
        try:
            info = self._client.vm_info(self._name)
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

    # ── exec ───────────────────────────────────────────────────────

    def exec(
        self,
        command: str | list[str],
        cwd: str = "/workdir",
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
            Working directory inside the guest.
        env:
            Extra environment variables prepended to the command.
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
                f"Sandbox {self._name} is not alive",
                sandbox_id=self._name,
            )

        # Normalise to list-of-strings.
        if isinstance(command, str):
            args = command.split()
        else:
            args = list(command)

        # Inject cwd / env via shell wrapping when needed.
        # (The guest agent does not yet accept cwd/env natively.)
        prefix_parts: list[str] = []
        if cwd != "/workdir":
            prefix_parts.append(f"cd {cwd}")
        if env:
            for k, v in env.items():
                prefix_parts.append(f"export {k}={v}")
        elif self.env:
            for k, v in self.env.items():
                prefix_parts.append(f"export {k}={v}")
        if prefix_parts:
            # Combine into a single shell invocation.
            inner = " ".join(args)
            args = ["sh", "-c", " && ".join(prefix_parts + [inner])]

        timeout_secs = timeout if timeout is not None else self._default_timeout

        try:
            resp = self._client.vm_exec(self._name, args, timeout_secs)
        except ClientError as e:
            msg = str(e)
            if "timeout" in msg.lower():
                raise SandboxTimeoutError(
                    msg, sandbox_id=self._name, engine_error=msg
                ) from e
            raise TerraError(
                msg, sandbox_id=self._name, engine_error=msg
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
                sandbox_id=self._name,
            )

        return result

    # ── lifecycle ──────────────────────────────────────────────────

    def kill(self) -> None:
        """Stop and deregister the sandbox immediately.

        Idempotent — safe to call multiple times.
        """
        if not self._alive:
            return
        try:
            self._client.vm_destroy(self._name)
        except ClientError:
            pass
        finally:
            self._alive = False

    def metrics(self) -> dict:
        """Query current resource usage.

        Returns a dict with keys ``cpu_count`` and ``memory_mb``.
        """
        if not self._alive:
            raise SandboxStateError(
                f"Sandbox {self._name} is not alive",
                sandbox_id=self._name,
            )
        info = self._client.vm_info(self._name)
        return {
            "cpu_count": info.get("cpus"),
            "memory_mb": info.get("memory_mb"),
        }

    def resize(self, cpu: int | None = None, memory_mb: int | None = None) -> None:
        """Resize sandbox resources online (no reboot required).

        Parameters
        ----------
        cpu:
            New vCPU count.
        memory_mb:
            New memory in MiB.
        """
        if not self._alive:
            raise SandboxStateError(
                f"Sandbox {self._name} is not alive",
                sandbox_id=self._name,
            )
        kwargs: dict = {}
        if cpu is not None:
            kwargs["cpus"] = cpu
        if memory_mb is not None:
            kwargs["memory_bytes"] = memory_mb * 1024 * 1024
        self._client.vm_resize(self._name, **kwargs)

    # ── context manager ────────────────────────────────────────────

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, *args: object) -> None:
        self.kill()

    def __repr__(self) -> str:
        status = self.status if self._alive else "stopped"
        return f"Sandbox(id={self._name!r}, status={status!r})"

    def __del__(self) -> None:
        """Best-effort cleanup on GC."""
        if self._alive:
            try:
                self.kill()
            except Exception:
                pass

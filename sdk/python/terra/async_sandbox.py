"""Async wrapper around Sandbox for use in asyncio applications.

Usage::

    from terra.async_sandbox import AsyncSandbox

    # Async construction (recommended for asyncio apps):
    sb = await AsyncSandbox.create(template="py312")

    # Or from a sync context:
    sb = AsyncSandbox(template="py312")

    result = await sb.exec(["python3", "-c", "print(2+2)"])
    print(result.stdout)
    await sb.kill()

    # Async context manager — auto-kills on exit:
    async with await AsyncSandbox.create(template="py312") as sb:
        print((await sb.exec(["python3", "--version"])).stdout)
"""

from __future__ import annotations

import asyncio

from .sandbox import Sandbox
from .exceptions import ExecResult, ExecError, SandboxTimeoutError, SandboxStateError


class AsyncSandbox:
    """Async wrapper around :class:`~terra.sandbox.Sandbox`.

    Uses a thread-pool executor for blocking operations so the event
    loop stays free.  The API mirrors Sandbox::

        AsyncSandbox.exec(...)   ~  Sandbox.exec(...)
        AsyncSandbox.kill()      ~  Sandbox.kill()
        AsyncSandbox.files       ~  Sandbox.files
        AsyncSandbox.status      ~  Sandbox.status

    Parameters are identical to :class:`~terra.sandbox.Sandbox`.
    """

    # ── construction ──────────────────────────────────────────────

    def __init__(self, *args, **kwargs):
        """Synchronous constructor — creates the backing Sandbox inline.

        For asyncio applications prefer :meth:`create` which offloads
        the blocking VM-creation work to a thread pool.
        """
        self._sync = Sandbox(*args, **kwargs)

    @classmethod
    async def create(cls, *args, **kwargs) -> "AsyncSandbox":
        """Async-friendly constructor — creates the backing Sandbox in an
        executor thread so the event loop is never blocked during VM boot.

        Parameters are identical to :class:`~terra.sandbox.Sandbox`.
        """
        loop = asyncio.get_running_loop()
        sb = await loop.run_in_executor(None, lambda: Sandbox(*args, **kwargs))
        wrapper = cls.__new__(cls)
        wrapper._sync = sb
        return wrapper

    # ── properties ─────────────────────────────────────────────────

    @property
    def id(self) -> str:
        """Unique sandbox identifier (the VM name)."""
        return self._sync.id

    @property
    def status(self) -> str:
        """Current sandbox status (``"running"``, ``"stopped"``, etc.)."""
        return self._sync.status

    @property
    def backend(self) -> str:
        """Backend in use (``"ch"`` for Cloud Hypervisor)."""
        return self._sync.backend

    @property
    def files(self):
        """File operations client (sync for now — access from a thread if
        needed).

        Returns the same :class:`~terra.sandbox.FilesClient` as the
        underlying :class:`Sandbox`.
        """
        return self._sync.files

    @property
    def metadata(self) -> dict:
        """Arbitrary user metadata dictionary."""
        return self._sync.metadata

    @metadata.setter
    def metadata(self, value: dict) -> None:
        self._sync.metadata = value

    @property
    def env(self) -> dict[str, str]:
        """Default environment variables for :meth:`exec`."""
        return self._sync.env

    @env.setter
    def env(self, value: dict[str, str]) -> None:
        self._sync.env = value

    # ── exec ───────────────────────────────────────────────────────

    async def exec(
        self,
        command: str | list[str],
        cwd: str = "/workdir",
        env: dict[str, str] | None = None,
        timeout: int | None = None,
        check: bool = False,
    ) -> ExecResult:
        """Execute a command inside the sandbox asynchronously.

        Parameters are identical to :meth:`Sandbox.exec`.
        """
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(
            None,
            lambda: self._sync.exec(
                command, cwd=cwd, env=env, timeout=timeout, check=check
            ),
        )

    # ── lifecycle ──────────────────────────────────────────────────

    async def kill(self) -> None:
        """Stop and deregister the sandbox asynchronously.

        Idempotent — safe to call multiple times.
        """
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(None, self._sync.kill)

    async def metrics(self) -> dict:
        """Query current resource usage asynchronously.

        Returns a dict with keys ``cpu_count`` and ``memory_mb``.
        """
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(None, self._sync.metrics)

    async def resize(self, cpu: int | None = None, memory_mb: int | None = None) -> None:
        """Resize sandbox resources online (no reboot required).

        Parameters are identical to :meth:`Sandbox.resize`.
        """
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(
            None,
            lambda: self._sync.resize(cpu=cpu, memory_mb=memory_mb),
        )

    # ── async context manager ──────────────────────────────────────

    async def __aenter__(self) -> "AsyncSandbox":
        return self

    async def __aexit__(self, *args: object) -> None:
        await self.kill()

    # ── repr ───────────────────────────────────────────────────────

    def __repr__(self) -> str:
        return repr(self._sync)

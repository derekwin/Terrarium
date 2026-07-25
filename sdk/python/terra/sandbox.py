"""Sandbox operations — the first-class sandbox object."""

from __future__ import annotations

import time

from .client import TerraClient


class ExecResult:
    """Result of a sandbox command execution."""

    def __init__(self, data: dict):
        self.stdout: str = data.get("stdout", "")
        self.stderr: str = data.get("stderr", "")
        self.exit_code: int = data.get("exit_code", -1)

    @property
    def success(self) -> bool:
        return self.exit_code == 0

    def __repr__(self) -> str:
        return f"ExecResult(exit={self.exit_code}, stdout_len={len(self.stdout)})"


class Sandbox:
    """An isolated execution environment inside a VM."""

    def __init__(self, name: str, client: TerraClient, vm_name: str = ""):
        self.name = name
        self._client = client
        self.vm_name = vm_name

    def __repr__(self) -> str:
        return f"Sandbox(name={self.name!r})"

    def exec(
        self,
        *args: str,
        work_dir: str | None = None,
        env: dict[str, str] | None = None,
        memory_mb: int | None = None,
        cpu_shares: int | None = None,
    ) -> ExecResult:
        """Execute a command in the sandbox.

        Args:
            *args: Command and arguments as separate tokens.
            work_dir: Working directory inside the sandbox.
            env: Environment variables (secrets injection).
            memory_mb: Memory limit in MB.
            cpu_shares: CPU weight (1024 = default).
        """
        resp = self._client.sandbox_exec(
            list(args),
            work_dir=work_dir,
            env=env,
            memory_mb=memory_mb,
            cpu_shares=cpu_shares,
        )
        if resp.get("status") != "ok":
            raise RuntimeError(f"exec failed: {resp.get('message', resp)}")
        return ExecResult(resp.get("data", {}))

    def read_file(self, path: str) -> str:
        """Read a file from inside the sandbox."""
        resp = self._client.sandbox_read_file(path)
        if resp.get("status") != "ok":
            raise RuntimeError(f"read_file failed: {resp.get('message', resp)}")
        return resp.get("data", {}).get("content", "")

    def write_file(self, path: str, content: str) -> None:
        """Write a file into the sandbox."""
        resp = self._client.sandbox_write_file(path, content)
        if resp.get("status") != "ok":
            raise RuntimeError(f"write_file failed: {resp.get('message', resp)}")

    def list_dir(self, path: str = ".") -> list[str]:
        """List directory contents inside the sandbox."""
        resp = self._client.sandbox_list_dir(path)
        if resp.get("status") != "ok":
            raise RuntimeError(f"list_dir failed: {resp.get('message', resp)}")
        return resp.get("data", {}).get("entries", [])

    def __enter__(self) -> Sandbox:
        return self

    def __exit__(self, *args) -> None:
        pass  # Sandbox lifecycle managed by the engine


def create(
    name: str,
    *,
    tools: list[str] | None = None,
    memory_mb: int | None = None,
    cpu_shares: int | None = None,
    env: dict[str, str] | None = None,
    client: TerraClient | None = None,
) -> Sandbox:
    """Create a new sandbox.

    Args:
        name: Unique sandbox name.
        tools: Pre-installed tools (python, nodejs).
        memory_mb: Memory limit.
        cpu_shares: CPU weight.
        env: Per-sandbox environment variables.
    """
    if client is None:
        client = TerraClient()
    # Sandbox is created via the engine daemon — in M2, this goes through
    # the controller which picks a running VM and calls the sandboxd adapter.
    # For now, sandbox exec auto-initializes on first use.
    return Sandbox(name=name, client=client)

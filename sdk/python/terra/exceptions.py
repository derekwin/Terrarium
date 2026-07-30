"""Exception hierarchy for the Terra SDK.

Provides structured error types that carry contextual metadata
(sandbox id, engine error, exec result) for programmatic handling.

Usage::

    from terra.exceptions import TerraError, ExecError, SandboxTimeoutError

    try:
        vm.exec(["some-command"])
    except ExecError as e:
        print(f"exit_code={e.exec_result.exit_code} stderr={e.exec_result.stderr}")
    except SandboxTimeoutError:
        print("command timed out")
    except TerraError as e:
        print(f"engine error: {e.engine_error} on sandbox {e.sandbox_id}")
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Optional


@dataclass
class ExecResult:
    """Result of a command executed inside a sandbox."""

    exit_code: int
    stdout: str = ""
    stderr: str = ""


class TerraError(Exception):
    """Base exception for all Terra SDK errors.

    Attributes:
        sandbox_id: The sandbox on which the error occurred, if any.
        engine_error: The raw error string returned by the engine daemon.
    """

    def __init__(
        self,
        message: str,
        sandbox_id: Optional[str] = None,
        engine_error: Optional[str] = None,
    ):
        self.sandbox_id = sandbox_id
        self.engine_error = engine_error
        super().__init__(message)


class EngineError(TerraError):
    """Engine daemon-level error (daemon start, protocol, transport)."""


class BuildError(TerraError):
    """Layer / image / kernel build or resolution failure."""


class SandboxError(TerraError):
    """Base for errors related to a specific sandbox / VM."""


class SandboxTimeoutError(SandboxError):
    """A sandbox operation exceeded its deadline."""


class SandboxStateError(SandboxError):
    """Operation on a sandbox in an invalid state (shut down, recycled, etc.)."""


class ResourceError(SandboxError):
    """Sandbox resource exhaustion (OOM, disk full, etc.)."""


class ExecError(SandboxError):
    """Command execution inside a sandbox failed.

    Attributes:
        exec_result: Structured result (exit code, stdout, stderr, etc.).
    """

    def __init__(
        self,
        message: str,
        exec_result: ExecResult,
        **kwargs,
    ):
        self.exec_result = exec_result
        super().__init__(message, **kwargs)

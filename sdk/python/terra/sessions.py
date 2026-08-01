"""Session — a background exec session inside a sandbox (engine-tracked)."""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .sandbox import Sandbox


class Session:
    """A background exec session inside a sandbox (engine-tracked).

    Returned by :meth:`Sandbox.exec(..., background=True)`. The engine
    tracks the session until it finishes or is killed; this handle can
    query its status or kill it at any time.
    """

    def __init__(self, sandbox: "Sandbox", session_id: str, sandbox_id: str):
        self._sandbox = sandbox
        self._client = sandbox._client
        self.session_id = session_id
        self.sandbox_id = sandbox_id

    def status(self) -> dict:
        """Return the engine-tracked session status.

        Status is one of ``running`` / ``killed`` / ``terminated`` /
        ``completed`` / ``failed``. Raises TerraError only for an
        unknown session id (e.g. a stale handle after an engine
        restart) — finished sessions keep their record and report the
        terminal status with their captured output.
        """
        return self._client.session_status(self.session_id)

    def kill(self) -> dict:
        """Kill the session (engine: killpg in the guest)."""
        return self._client.session_kill(self.session_id)

    def __repr__(self) -> str:
        return f"Session(id={self.session_id!r}, sandbox={self.sandbox_id!r})"

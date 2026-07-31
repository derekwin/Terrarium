"""Internal daemon manager — auto-starts the engine when needed.

SDK users never touch this directly. It provides the lazy-start plumbing
that powers `terra.direct` and the session context manager.

DaemonManager is a thin wrapper around :class:`terra.daemon.Daemon` that
adds idempotency (a responsive daemon is never restarted) and raises
:class:`EngineError` instead of :class:`DaemonError`.
"""

from __future__ import annotations


class EngineError(RuntimeError):
    """The engine daemon failed to start or respond.

    Raised when auto-start times out or the daemon socket is unreachable.
    """

    def __init__(self, message: str, *, engine_error: str | None = None):
        super().__init__(message)
        self.engine_error = engine_error


class DaemonManager:
    """Internal daemon lifecycle — auto-starts on first use.

    Usage (not for end users):

        mgr = DaemonManager()
        mgr.ensure_running()
        # ... use the daemon ...
        mgr.stop()
    """

    def __init__(self, socket_path: str | None = None):
        from .daemon import Daemon
        from .paths import default_socket

        self.socket_path: str = socket_path or default_socket()
        self._daemon = Daemon(socket=self.socket_path)

    # ── public API ──────────────────────────────────────────────

    def ensure_running(self, timeout: float = 10.0) -> None:
        """Ensure the engine daemon is running on *socket_path*.

        If the socket is already responsive this is a no-op; otherwise
        the daemon is started and we poll until it answers or *timeout*
        seconds elapse.
        """
        from .daemon import DaemonError, daemon_ping

        if daemon_ping(self.socket_path):
            return
        try:
            self._daemon.start(timeout=timeout)
        except DaemonError as e:
            raise EngineError(str(e), engine_error=str(e)) from e

    def health_check(self) -> bool:
        """Return *True* if the daemon is responsive."""
        from .daemon import daemon_ping

        return daemon_ping(self.socket_path)

    def stop(self) -> None:
        """Graceful shutdown — sends ``daemon_stop`` and checks the reply.

        An embedded (in-process) daemon refuses ``daemon_stop`` by
        design; that refusal is a no-op success because the daemon
        thread dies with its host process. An unreachable daemon is
        already gone. Any other engine error is raised.
        """
        from .daemon import DaemonError, daemon_stop

        try:
            daemon_stop(self.socket_path)
        except DaemonError as e:
            raise EngineError(
                f"daemon_stop failed: {e}", engine_error=str(e)
            ) from e

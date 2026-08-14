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
        import os as _os
        import time as _time

        from .daemon import DaemonError, daemon_ping

        if daemon_ping(self.socket_path):
            return
        # A socket file that exists but does not answer usually means the
        # daemon is BUSY (a heavy create burst can starve its accept loop)
        # or still starting. Auto-starting an embedded daemon here would
        # steal the socket path from a merely-busy root daemon and silently
        # lose CAP_NET_ADMIN. Retry the ping before falling back.
        if _os.path.exists(self.socket_path):
            deadline = _time.time() + min(float(timeout), 8.0)
            while _time.time() < deadline:
                if daemon_ping(self.socket_path):
                    return
                _time.sleep(0.25)
            # A socket file that never answers after the retry window means
            # the existing daemon is wedged/busy, not absent. Auto-starting
            # an embedded daemon would unlink its socket path and steal it
            # (silently losing root privileges / NAT). Surface the state
            # instead — the operator restarts the daemon.
            raise EngineError(
                f"engine daemon at {self.socket_path} exists but is "
                "unresponsive (busy or wedged) — stop it with "
                "'terra daemon stop' and retry",
                engine_error="daemon socket present but not answering",
            )
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

"""Internal daemon manager — auto-starts the engine when needed.

SDK users never touch this directly. It provides the lazy-start plumbing
that powers `terra.direct` and the session context manager.

Unlike `terra.daemon.Daemon` (which is a user-facing context manager
with full lifecycle control), DaemonManager is a lower-level building
block: fire-and-forget auto-start, health checks, graceful stop.
"""

from __future__ import annotations

import os
import socket as _socket
import time


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
        from .paths import default_socket

        self.socket_path: str = socket_path or default_socket()

    # ── public API ──────────────────────────────────────────────

    def ensure_running(self, timeout: float = 10.0) -> None:
        """Ensure the engine daemon is running on *socket_path*.

        If the socket is already responsive this is a no-op; otherwise
        the daemon is started and we poll until it answers or *timeout*
        seconds elapse.
        """
        if self._ping():
            return
        self._start_daemon()
        self._wait_ready(timeout)

    def health_check(self) -> bool:
        """Return *True* if the daemon is responsive."""
        return self._ping()

    def stop(self) -> None:
        """Graceful shutdown — sends ``shutdown all`` then returns.

        Errors are silently ignored (the daemon may already be gone).
        """
        try:
            s = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
            s.settimeout(5)
            s.connect(self.socket_path)
            s.sendall(b'{"command":"shutdown","name":"all"}\n')
            s.close()
        except Exception:
            pass

    # ── internals ───────────────────────────────────────────────

    def _ping(self) -> bool:
        """Send a lightweight ``list`` command and check for a reply.

        Returns *True* if the daemon answered with *any* data (we do not
        parse the response — a connected socket that sends bytes back is
        good enough).
        """
        try:
            s = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
            s.settimeout(1)
            s.connect(self.socket_path)
            s.sendall(b'{"command":"list"}\n')
            s.recv(1024)
            s.close()
            return True
        except (OSError, _socket.error):
            return False

    def _start_daemon(self) -> None:
        """Launch the engine daemon in-process via PyO3 FFI.

        The ``ch_binary`` path can be overridden with
        ``TERRA_CH_BINARY``; otherwise *terrarium_engine* resolves its
        own default.
        """
        import terrarium_engine

        ch_binary = os.environ.get("TERRA_CH_BINARY")
        terrarium_engine.start_daemon(self.socket_path, ch_binary=ch_binary)

    def _wait_ready(self, timeout: float) -> None:
        """Poll the socket until the daemon responds or *timeout* expires."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self._ping():
                return
            time.sleep(0.1)
        raise EngineError(
            f"Daemon did not start within {timeout}s",
            engine_error="startup timeout",
        )

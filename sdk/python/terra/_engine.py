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
        self._fix_socket_owner()

    def health_check(self) -> bool:
        """Return *True* if the daemon is responsive."""
        return self._ping()

    def stop(self) -> None:
        """Graceful shutdown — sends ``daemon_stop`` and checks the reply.

        An embedded (in-process) daemon refuses ``daemon_stop`` by
        design; that refusal is a no-op success because the daemon
        thread dies with its host process. An unreachable daemon is
        already gone. Any other engine error is raised.
        """
        from .client import TerraClient, TerraError
        from .daemon import EMBEDDED_STOP_REFUSAL

        try:
            TerraClient(socket_path=self.socket_path)._send({"command": "daemon_stop"})
        except TerraError as e:
            if EMBEDDED_STOP_REFUSAL in str(e):
                return  # embedded refusal — dies with the host process
            raise EngineError(
                f"daemon_stop failed: {e}", engine_error=str(e)
            ) from e
        except (OSError, TimeoutError):
            pass  # daemon already gone

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

        The environment (state/layer dirs, CH/virtiofsd binaries,
        default kernel + agent initramfs) comes from the shared
        :func:`terra.daemon.build_daemon_env`, so Sandbox-started
        daemons see the same managed assets as ``terra daemon start``
        ones. ``embedded`` keeps its fail-safe default (True): an
        in-process daemon refuses ``daemon_stop``.
        """
        import terrarium_engine

        from .daemon import build_daemon_env

        env = build_daemon_env()
        os.environ.update(env)
        terrarium_engine.start_daemon(
            self.socket_path, ch_binary=env["TERRA_CH_BINARY"]
        )

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

    def _fix_socket_owner(self) -> None:
        """When daemon is started via sudo, chown socket to original user."""
        uid = os.environ.get("SUDO_UID")
        gid = os.environ.get("SUDO_GID")
        if uid and gid:
            try:
                os.chown(self.socket_path, int(uid), int(gid))
            except OSError:
                pass  # best-effort: socket might already be usable

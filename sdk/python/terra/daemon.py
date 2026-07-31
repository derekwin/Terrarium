"""One-command engine daemon lifecycle for SDK users.

    from terra.daemon import Daemon
    from terra.client import TerraClient

    with Daemon() as d:
        client = TerraClient(socket_path=d.socket)
        ...

The engine daemon runs in-process via PyO3 FFI (terrarium_engine Rust
crate). No subprocess spawning. Requires ``maturin develop`` to have
been run first in the crate workspace.

Everything (CH, virtiofsd, state/layer dirs, socket) is resolved and
injected automatically — zero environment variables needed.
"""

from __future__ import annotations

import os
import socket as _socket
import time
from pathlib import Path

from . import assets, images, paths
from .client import TerraClient, TerraError as _ClientError
from .config import HostConfig


class DaemonError(RuntimeError):
    """The daemon failed to start or stop."""


# Engine error substring returned when daemon_stop is sent to an
# embedded (in-process) daemon — see crates/engine/src/daemon.rs.
EMBEDDED_STOP_REFUSAL = "not supported in embedded mode"


def build_daemon_env(
    config: HostConfig | None = None,
    *,
    kernel: str | None = None,
    ch_binary: str | None = None,
    virtiofsd: str | None = None,
    layer_dir: str | None = None,
    state_dir: str | None = None,
) -> dict[str, str]:
    """Resolve the full daemon environment — single source of truth.

    Used by both :meth:`Daemon.start` and the internal DaemonManager so
    every daemon sees the managed state/layer dirs, host binaries and
    default guest images, regardless of which path started it.
    """
    cfg = config or HostConfig()
    env = cfg.env()
    env["TERRA_STATE_DIR"] = state_dir or cfg.state_dir or str(paths.state_dir())
    env["TERRA_LAYER_DIR"] = layer_dir or cfg.layer_dir or str(paths.layers_dir())
    env["TERRA_CH_BINARY"] = ch_binary or cfg.ch_binary or str(assets.ensure_ch())
    env["TERRA_VIRTIOFSD"] = virtiofsd or cfg.virtiofsd or str(assets.ensure_virtiofsd())
    kernel = kernel or cfg.kernel
    if kernel:
        env["TERRA_KERNEL"] = str(Path(kernel).expanduser())
    # Best-effort image defaults so engine pool_create doesn't fall
    # back to repo-relative target/guest paths. Skipped silently when
    # no default images are resolvable yet — explicit per-VM
    # kernel/initramfs still work without them.
    if "TERRA_KERNEL" not in env:
        try:
            env["TERRA_KERNEL"] = str(images.ensure("vmlinux.bin"))
        except Exception:  # noqa: BLE001
            pass
    if "TERRA_AGENT_INITRAMFS" not in env:
        try:
            env["TERRA_AGENT_INITRAMFS"] = str(images.ensure("initramfs-agent.cpio.gz"))
        except Exception:  # noqa: BLE001
            pass
    # Managed bin dir on PATH: the engine's fallback tool lookups
    # (erofsfuse, virtiofsd, mkfs.erofs — see crates/fs/src/erofs.rs)
    # resolve bare binary names via PATH.
    bin_dir = str(paths.bin_dir())
    if bin_dir not in os.environ.get("PATH", "").split(os.pathsep):
        env["PATH"] = bin_dir + os.pathsep + os.environ.get("PATH", "")
    return env


def daemon_ping(socket_path: str) -> bool:
    """Return *True* if a daemon on *socket_path* answers a lightweight ``list`` command.

    A connected socket that sends bytes back is good enough — the
    response is not parsed. Used by :meth:`Daemon.start` readiness
    polling and by the internal DaemonManager.
    """
    try:
        s = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        s.settimeout(1)
        s.connect(socket_path)
        s.sendall(b'{"command":"list"}\n')
        s.recv(1024)
        s.close()
        return True
    except OSError:
        return False


def fix_socket_owner(socket_path: str, timeout: float = 5.0) -> None:
    """Best-effort chown of the daemon socket to the original user.

    When the daemon was started via sudo, chown the socket so regular
    CLI commands work without sudo. Polls up to *timeout* seconds for
    the socket file to appear after launch.
    """
    uid = os.environ.get("SUDO_UID")
    gid = os.environ.get("SUDO_GID")
    if uid and gid:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if Path(socket_path).exists():
                try:
                    os.chown(socket_path, int(uid), int(gid))
                except OSError:
                    pass
                break
            time.sleep(0.05)


def daemon_stop(socket_path: str) -> str:
    """Send the ``daemon_stop`` wire command and classify the outcome.

    Returns one of:
    - ``"stopped"`` — the daemon acknowledged and is shutting down;
    - ``"refused"`` — an embedded (in-process) daemon refused the stop
      by design (it dies with its host process);
    - ``"gone"`` — the daemon was unreachable (already shut down).

    Any other engine error is raised as :class:`DaemonError`.
    """
    try:
        TerraClient(socket_path=socket_path)._send({"command": "daemon_stop"})
    except _ClientError as e:
        if EMBEDDED_STOP_REFUSAL not in str(e):
            raise DaemonError(f"daemon_stop failed: {e}") from e
        return "refused"
    except (OSError, TimeoutError):
        return "gone"
    return "stopped"


class Daemon:
    """Manage an engine daemon via in-process Rust FFI (PyO3)."""

    def __init__(
        self,
        *,
        config: HostConfig | None = None,
        socket: str | None = None,
        tcp: str | None = None,
        kernel: str | None = None,
        ch_binary: str | None = None,
        virtiofsd: str | None = None,
        layer_dir: str | None = None,
        state_dir: str | None = None,
        log: str | None = None,
        embedded: bool = True,
    ):
        self.socket = socket or paths.default_socket()
        self.tcp = tcp
        self.config = config or HostConfig()
        self._kernel = kernel or self.config.kernel
        self._ch = ch_binary or self.config.ch_binary
        self._vfsd = virtiofsd or self.config.virtiofsd
        self._layer_dir = layer_dir or self.config.layer_dir
        self._state_dir = state_dir or self.config.state_dir
        self._log = log
        # embedded=True (fail-safe default): the daemon runs inside a
        # host process and refuses daemon_stop. Pass False only when
        # this process is a dedicated daemon (e.g. the subprocess
        # spawned by `terra daemon start`).
        self._embedded = embedded
        self._log_file = None

    def start(self, timeout: float = 5.0) -> "Daemon":
        env_updates = build_daemon_env(
            self.config,
            kernel=self._kernel,
            ch_binary=self._ch,
            virtiofsd=self._vfsd,
            layer_dir=self._layer_dir,
            state_dir=self._state_dir,
        )
        os.environ.update(env_updates)
        ch_binary = env_updates["TERRA_CH_BINARY"]

        if Path(self.socket).exists():
            try:
                Path(self.socket).unlink()
            except PermissionError:
                self.socket = str(paths.run_dir() / f"terra-{os.getpid()}.sock")
        if self._log:
            self._log_file = open(self._log, "w")

        import terrarium_engine

        terrarium_engine.start_daemon(
            self.socket, ch_binary=ch_binary, embedded=self._embedded
        )

        # When started via sudo, chown the socket to the original user
        # so regular CLI commands work without sudo.
        fix_socket_owner(self.socket, timeout=timeout)

        deadline = time.time() + timeout
        while time.time() < deadline:
            if daemon_ping(self.socket):
                return self
            time.sleep(0.1)
        raise DaemonError(f"daemon socket did not appear within {timeout}s")

    def stop(self, timeout: float = 15.0) -> None:
        """Stop the daemon via the ``daemon_stop`` wire command.

        A service daemon (``embedded=False``) acknowledges, shuts down
        all VMs and exits — we then wait for its process to disappear
        and remove the pidfile. An embedded (in-process) daemon refuses
        ``daemon_stop`` by design; that refusal is a no-op success here
        because the daemon thread dies with its host process. An
        unreachable daemon is already gone. Any other engine error is
        raised as :class:`DaemonError`.
        """
        if daemon_stop(self.socket) == "stopped":
            self._reap_service_daemon(timeout)
        if self._log_file:
            self._log_file.close()
            self._log_file = None

    def _reap_service_daemon(self, timeout: float) -> None:
        """Wait for the daemon subprocess to exit and drop pidfile state.

        The pidfile is a `terra daemon start` convention for the
        default-socket service daemon — custom-socket daemons have no
        pidfile to clean up.
        """
        if self.socket == paths.default_socket():
            pidfile = paths.run_dir() / "daemon.pid"
            try:
                pid = int(pidfile.read_text().strip())
            except (OSError, ValueError):
                pid = None
            if pid is not None:
                deadline = time.time() + timeout
                while time.time() < deadline:
                    try:
                        # A zombie has exited but not yet been reaped.
                        gone = Path(f"/proc/{pid}/stat").read_text().split()[2] == "Z"
                    except (OSError, IndexError):
                        gone = True
                    if gone:
                        break
                    time.sleep(0.1)
            pidfile.unlink(missing_ok=True)
        # Remove a stale socket file if the engine didn't.
        try:
            Path(self.socket).unlink()
        except FileNotFoundError:
            pass

    def __enter__(self) -> "Daemon":
        return self.start()

    def __exit__(self, *args: object) -> None:
        self.stop()


from contextlib import contextmanager


@contextmanager
def session(**daemon_kwargs):
    """Direct mode: a temporary engine daemon + client, zero setup.

        with terra.session() as c:
            print(c.vm_exec(...))

    The daemon starts on entry and is torn down on exit. Keyword args
    are forwarded to Daemon().
    """
    with Daemon(**daemon_kwargs) as d:
        yield TerraClient(socket_path=d.socket)

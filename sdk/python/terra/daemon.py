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

from . import assets, paths
from .config import HostConfig


class DaemonError(RuntimeError):
    """The daemon failed to start or stop."""


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
        self._log_file = None

    def start(self, timeout: float = 5.0) -> "Daemon":
        ch_binary = self._ch or str(assets.ensure_ch())
        env_updates = {
            **self.config.env(),
            "TERRA_STATE_DIR": self._state_dir or str(paths.state_dir()),
            "TERRA_CH_BINARY": ch_binary,
            "TERRA_LAYER_DIR": self._layer_dir or str(paths.layers_dir()),
        }
        vfsd = self._vfsd or str(assets.ensure_virtiofsd())
        env_updates["TERRA_VIRTIOFSD"] = vfsd
        if self._kernel:
            env_updates["TERRA_KERNEL"] = self._kernel
        os.environ.update(env_updates)

        if Path(self.socket).exists():
            try:
                Path(self.socket).unlink()
            except PermissionError:
                self.socket = str(paths.run_dir() / f"terra-{os.getpid()}.sock")
        if self._log:
            self._log_file = open(self._log, "w")

        import terrarium_engine

        terrarium_engine.start_daemon(self.socket, ch_binary=ch_binary)

        # When started via sudo, chown the socket to the original user
        # so regular CLI commands work without sudo.
        uid = os.environ.get("SUDO_UID")
        gid = os.environ.get("SUDO_GID")
        if uid and gid:
            deadline = time.time() + timeout
            while time.time() < deadline:
                if Path(self.socket).exists():
                    try:
                        os.chown(self.socket, int(uid), int(gid))
                    except OSError:
                        pass
                    break
                time.sleep(0.05)

        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                s = _socket.socket(_socket.AF_UNIX)
                s.connect(self.socket)
                s.close()
                return self
            except (ConnectionRefusedError, FileNotFoundError):
                time.sleep(0.1)
        raise DaemonError(f"daemon socket did not appear within {timeout}s")

    def stop(self, timeout: float = 15.0) -> None:
        try:
            s = _socket.socket(_socket.AF_UNIX)
            s.settimeout(5)
            s.connect(self.socket)
            s.sendall(b'{"command":"shutdown","name":"all"}\n')
            s.close()
        except Exception:
            pass
        if self._log_file:
            self._log_file.close()
            self._log_file = None

    def __enter__(self) -> "Daemon":
        return self.start()

    def __exit__(self, *args: object) -> None:
        self.stop()


from contextlib import contextmanager

from .client import TerraClient


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

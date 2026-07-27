"""One-command engine daemon lifecycle for SDK users.

    from terra.daemon import Daemon
    from terra.client import TerraClient

    with Daemon() as d:
        client = TerraClient(socket_path=d.socket)
        ...

Everything (engine binary, CH, virtiofsd, state/layer dirs, socket) is
resolved and injected automatically — zero environment variables needed.
"""

from __future__ import annotations

import os
import signal
import subprocess
import time
from pathlib import Path

from . import assets, paths


class DaemonError(RuntimeError):
    """The daemon failed to start or stop."""


class Daemon:
    """Manage an engine daemon process with a fully managed environment."""

    def __init__(
        self,
        *,
        socket: str | None = None,
        kernel: str | None = None,
        ch_binary: str | None = None,
        virtiofsd: str | None = None,
        layer_dir: str | None = None,
        state_dir: str | None = None,
        log: str | None = None,
    ):
        self.socket = socket or paths.default_socket()
        self._kernel = kernel
        self._ch = ch_binary
        self._vfsd = virtiofsd
        self._layer_dir = layer_dir
        self._state_dir = state_dir
        self._log = log
        self._proc: subprocess.Popen | None = None
        self._log_file = None

    def start(self, timeout: float = 15.0) -> "Daemon":
        engine = assets.ensure_engine()
        env = {
            **os.environ,
            "TERRA_STATE_DIR": self._state_dir or str(paths.state_dir()),
            "TERRA_CH_BINARY": self._ch or str(assets.ensure_ch()),
            "TERRA_LAYER_DIR": self._layer_dir or str(paths.layers_dir()),
        }
        vfsd = self._vfsd or str(assets.ensure_virtiofsd())
        env["TERRA_VIRTIOFSD"] = vfsd
        if self._kernel:
            env["TERRA_KERNEL"] = self._kernel

        if Path(self.socket).exists():
            Path(self.socket).unlink()
        if self._log:
            self._log_file = open(self._log, "w")
            out, err = self._log_file, subprocess.STDOUT
        else:
            out = err = subprocess.DEVNULL
        self._proc = subprocess.Popen(
            [str(engine), "daemon", "--socket", self.socket],
            env=env, stdout=out, stderr=err,
        )
        deadline = time.time() + timeout
        while time.time() < deadline:
            if Path(self.socket).exists():
                return self
            if self._proc.poll() is not None:
                raise DaemonError(f"engine daemon exited early (rc={self._proc.returncode})")
            time.sleep(0.2)
        raise DaemonError(f"daemon socket did not appear within {timeout}s")

    def stop(self, timeout: float = 15.0) -> None:
        if self._proc is None:
            return
        self._proc.send_signal(signal.SIGTERM)
        try:
            self._proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait(timeout=5)
        self._proc = None
        if self._log_file:
            self._log_file.close()

    @property
    def pid(self) -> int | None:
        return self._proc.pid if self._proc else None

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

    The daemon starts on entry and is torn down (SIGTERM, VMs cleaned)
    on exit. Keyword args are forwarded to Daemon().
    """
    with Daemon(**daemon_kwargs) as d:
        yield TerraClient(socket_path=d.socket)

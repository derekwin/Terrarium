"""Batch environment orchestration (P1-2) — RL/eval episode scaling.

A :class:`Batch` is a set of environments restored from one snapshot (the
P1 fast-reset primitive — docs/plans/2026-08-03-snapshot-reset.md):

    with Batch("/tmp/env-ready", 16, layers=["base"]) as envs:
        results = envs.collect(["run-task", "--input", "x"])
        envs.recycle()          # deterministic episode reset
        results = envs.collect(["run-task", "--input", "y"])

``collect`` runs a command in every environment in parallel;
``report`` returns a host density snapshot (count, per-VM RSS/Pss).

Restore/destroy run concurrently client-side, and the daemon runs VM
lifecycle (create/restore) lock-free: 8 concurrent restores measured at
~230 ms on real KVM (vs ~210 ms each serialized before the lock-free
path).
"""

from __future__ import annotations

import concurrent.futures
from typing import Any

from .client import TerraClient


def _proc_memory_mb(pid: int) -> tuple[float, float] | None:
    """(rss_mb, pss_mb) for a host process, or None if unreadable.
    Pss from smaps_rollup counts shared pages once across processes."""
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            rss = pss = None
            for line in f:
                if line.startswith("Rss:"):
                    rss = int(line.split()[1]) / 1024.0
                elif line.startswith("Pss:"):
                    pss = int(line.split()[1]) / 1024.0
            if rss is not None and pss is not None:
                return rss, pss
    except (OSError, ValueError):
        pass
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    rss = int(line.split()[1]) / 1024.0
                    return rss, rss
    except (OSError, ValueError):
        pass
    return None


class Batch:
    """A set of environments restored from one snapshot."""

    def __init__(
        self,
        snapshot_path: str,
        size: int,
        *,
        layers: list[str],
        prefix: str = "env",
        net: bool = False,
        workers: int = 16,
    ):
        self._client = TerraClient()
        self._snapshot = snapshot_path
        self._layers = list(layers)
        self._net = bool(net)
        self._names = [f"{prefix}-{i}" for i in range(size)]
        self._workers = max(1, min(workers, size))
        self._restore_all()

    # -- lifecycle ----------------------------------------------------------

    def _restore_one(self, name: str) -> None:
        self._client.vm_restore(name, self._snapshot, layers=self._layers, net=self._net)

    def _restore_all(self) -> None:
        with concurrent.futures.ThreadPoolExecutor(max_workers=self._workers) as ex:
            list(ex.map(self._restore_one, self._names))

    def recycle(self) -> None:
        """Deterministic episode reset: destroy and re-restore every env
        from the same snapshot."""
        self.destroy()
        self._restore_all()

    def reset_in_place(self) -> None:
        """Fast episode reset (P1/RL): each env stays running — the guest
        kills its episode processes and clears the episode-writable
        runtime dirs back to the LAYER baseline. ~10x cheaper than
        recycle; env ready-state must live in a layer (episode writes go
        to the upper)."""
        with concurrent.futures.ThreadPoolExecutor(max_workers=self._workers) as ex:
            list(ex.map(self._client.vm_reset, self._names))

    def destroy(self) -> None:
        with concurrent.futures.ThreadPoolExecutor(max_workers=self._workers) as ex:
            list(ex.map(self._client.vm_destroy, self._names))

    def names(self) -> list[str]:
        return list(self._names)

    # -- operations ---------------------------------------------------------

    def exec(self, args: list[str], timeout_secs: int = 60) -> dict[str, dict[str, Any]]:
        """Run ``args`` in every environment in parallel."""

        def run(name: str) -> tuple[str, dict[str, Any]]:
            return name, self._client.vm_exec(name, args, timeout_secs=timeout_secs)

        with concurrent.futures.ThreadPoolExecutor(max_workers=self._workers) as ex:
            return dict(ex.map(run, self._names))

    def collect(self, args: list[str], timeout_secs: int = 60) -> dict[str, str]:
        """Run ``args`` in every env; return ``{name: stdout}``."""
        return {
            name: result.get("stdout", "")
            for name, result in self.exec(args, timeout_secs).items()
        }

    def report(self) -> dict[str, Any]:
        """Host density snapshot: env count, per-VM RSS/Pss (best-effort)."""
        vms = self._client.vm_list().get("vms", [])
        rss = pss = 0.0
        seen = False
        for vm in vms:
            pid = vm.get("pid")
            if not pid:
                continue
            m = _proc_memory_mb(pid)
            if m is None:
                continue
            seen = True
            rss += m[0]
            pss += m[1]
        return {
            "environments": len(self._names),
            "registered_vms": len(vms),
            "rss_mb": round(rss, 1) if seen else None,
            "pss_mb": round(pss, 1) if seen else None,
        }

    # -- context manager ----------------------------------------------------

    def __enter__(self) -> "Batch":
        return self

    def __exit__(self, *exc: object) -> None:
        self.destroy()

    def __len__(self) -> int:
        return len(self._names)

"""Warm-pool management — pre-booted idle VMs with fast hot-plug claim.

Usage::

    from terra.pool import Pool
    from terra.template import Template

    # Pool from a named template:
    pool = Pool(template="py312", size=3)

    # Pool from explicit layers:
    pool = Pool(layers=["python312", "base"], size=2)

    # Check status:
    print(pool.status())  # {"idle": 3, "claimed": 0, "total": 3}

    # Acquire a ready sandbox:
    sb = pool.acquire()
    result = sb.exec(["python3", "--version"])
    print(result.stdout)

    # Pool-claimed sandboxes can be released back:
    pool.release(sb)      # returns VM to idle pool
    sb.kill()             # removes only the sandbox workdir — the pool
                          # VM stays claimed (vm_destroy deregisters it)

    # Warm-pool VMs are pre-booted and have the layers hot-plugged on
    # claim.  This is much faster than cold-creating a Sandbox.
"""

from __future__ import annotations

import logging
from uuid import uuid4

from .client import TerraClient
from .sandbox import Sandbox, _SYSTEM_MAP
from .template import Template

_log = logging.getLogger(__name__)


class Pool:
    """A warm pool of pre-booted idle VMs.

    Pool VMs are created eagerly (``pool_create``) and sit idle until
    claimed.  On claim the requested layers are hot-plugged and the VM
    is returned as a :class:`~terra.sandbox.Sandbox` instance.

    Parameters
    ----------
    template:
        Named template to load (resolves *layers*).  Mutually replaces
        explicit *layers*.
    size:
        Number of idle VMs to keep ready (default 3).
    layers:
        Explicit virtiofs layer names for pool VMs.  Defaults to
        ``["base"]`` when neither *template* nor *layers* given.
    kernel:
        Kernel variant name or path for pool VMs.
    net:
        Truthy → pool VMs get virtio-net (NAT + DHCP).
    """

    def __init__(
        self,
        template: str | None = None,
        size: int = 3,
        *,
        layers: list[str] | None = None,
        kernel: str | None = None,
        net: bool = False,
    ):
        self._client = TerraClient()
        self._size = size

        # -- resolve template -------------------------------------------------
        if template:
            t = Template.load(template)
            system = _SYSTEM_MAP.get(t.base, t.base)
            if layers is None:
                layers = list(t.layers)
                # overlayfs rejects duplicate lower layers (ELOOP).
                if system not in layers:
                    layers.append(system)
            if kernel is None and t.kernel:
                kernel = t.kernel

        if layers is None:
            layers = ["base"]

        self._layers = layers
        self._kernel = kernel
        self._net = net
        self._create_pool()

    def _create_pool(self) -> None:
        """Create the warm-pool VMs (idempotent — re-creates if needed)."""
        resp = self._client.pool_create(
            self._size,
            kernel=self._kernel or None,
            net=self._net,
        )
        failed = resp.get("failed", [])
        if failed:
            _log.warning(
                "warm pool: %d of %d VMs never became ready: %s",
                len(failed), self._size,
                "; ".join(f"{f['name']}: {f['error']}" for f in failed),
            )

    # ── acquire / release ──────────────────────────────────────────

    def acquire(self) -> Sandbox:
        """Claim an idle pool VM and hot-plug the configured layers.

        Returns a :class:`~terra.sandbox.Sandbox` that is ready to use.
        The sandbox has been pre-booted and the layers are already
        attached. Pool sandboxes share the pool VM as their tenant VM.

        Raises
        ------
        TerraError
            When the pool is exhausted (no idle slots).
        """
        claim = self._client.pool_claim(self._layers)
        session_id = f"sb-{uuid4().hex[:4]}"
        workdir = f"/workdir/{session_id}"
        return Sandbox._from_claimed_vm(
            self._client, claim["name"], workdir, session_id
        )

    def release(self, sandbox: Sandbox) -> None:
        """Release a claimed pool VM back to idle.

        The VM stays running and can be claimed again later.  Note that
        :meth:`Sandbox.kill` only removes the sandbox workdir — it does
        NOT deregister the pool VM.  Use ``vm_destroy`` to deregister
        a VM permanently.
        """
        self._client.pool_release(sandbox._vm_name)
        sandbox._alive = False

    # ── status ─────────────────────────────────────────────────────

    def status(self) -> dict:
        """Query pool state.

        Returns a dict::

            {"idle": N, "claimed": N, "total": N}
        """
        result = self._client.pool_list()
        slots = result.get("pool", [])
        idle = sum(1 for s in slots if not s.get("claimed"))
        claimed = sum(1 for s in slots if s.get("claimed"))
        return {"idle": idle, "claimed": claimed, "total": len(slots)}

    # ── grow / shrink ──────────────────────────────────────────────

    def grow(self, count: int = 1) -> None:
        """Add *count* more idle VMs to the pool."""
        self._size += count
        self._client.pool_create(
            self._size,
            kernel=self._kernel or None,
            net=self._net,
        )

    # ── repr ───────────────────────────────────────────────────────

    def __repr__(self) -> str:
        st = self.status()
        return (
            f"Pool(layers={self._layers!r}, "
            f"idle={st['idle']}, claimed={st['claimed']}, total={st['total']})"
        )

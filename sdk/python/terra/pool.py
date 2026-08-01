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
from .exceptions import TerraError
from .sandbox import Sandbox, _SYSTEM_MAP
from .template import Template

_log = logging.getLogger(__name__)

# Engine cap: a single pool_create call spawns 1..=32 VMs.
_POOL_SIZE_MIN = 1
_POOL_SIZE_MAX = 32


def scale_pool(
    client: TerraClient,
    target: int,
    *,
    kernel: str | None = None,
    net: bool = False,
) -> dict:
    """Scale a warm pool to exactly *target* idle VMs.

    Module-level so both :class:`Pool` and the ``terra pool scale`` CLI
    share one implementation.  Reads the current idle count via
    ``pool_list`` and then:

    - short of target  → ``pool_create(target - idle)`` (delta only —
      the engine spawns exactly *N new* VMs per call),
    - over target      → ``vm_destroy`` surplus **idle** slots (never
      claimed ones; the engine deregisters the pool slot on destroy),
    - at target        → no-op.

    Returns a summary dict ``{"idle", "created", "destroyed"}``.
    """
    if not _POOL_SIZE_MIN <= target <= _POOL_SIZE_MAX:
        raise TerraError(
            f"pool size must be between {_POOL_SIZE_MIN} and "
            f"{_POOL_SIZE_MAX} (got {target})"
        )
    slots = client.pool_list().get("pool", [])
    idle = [s["name"] for s in slots if not s.get("claimed")]
    current = len(idle)
    created: list[str] = []
    destroyed: list[str] = []
    if current < target:
        resp = client.pool_create(target - current, kernel=kernel, net=net)
        created = list(resp.get("created", []))
    elif current > target:
        destroyed = idle[: current - target]
        for name in destroyed:
            client.vm_destroy(name)
    return {"idle": target, "created": created, "destroyed": destroyed}


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
        """Add *count* more idle VMs to the pool.

        Only the delta is created — the engine spawns exactly *N new*
        VMs per ``pool_create`` call, never a running total.
        """
        self._size += count
        self._client.pool_create(
            count,
            kernel=self._kernel or None,
            net=self._net,
        )

    def scale(self, target: int) -> None:
        """Scale the pool to exactly *target* idle VMs.

        Creates the shortfall or destroys surplus idle VMs (claimed
        slots are never touched).  Valid targets: 1..=32.
        """
        scale_pool(self._client, target, kernel=self._kernel, net=self._net)
        self._size = target

    # ── repr ───────────────────────────────────────────────────────

    def __repr__(self) -> str:
        st = self.status()
        return (
            f"Pool(layers={self._layers!r}, "
            f"idle={st['idle']}, claimed={st['claimed']}, total={st['total']})"
        )

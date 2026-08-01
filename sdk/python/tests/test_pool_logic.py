"""Pool grow/scale logic — client mocked, no VMs.

Engine semantics under test:
- ``pool_create(N)`` spawns *N new* VMs (it never knows the existing
  pool size), so ``grow`` must pass the delta, not the new total.
- ``scale(target)`` must reach *exactly* ``target`` idle slots: create
  the delta when short, destroy surplus **idle** slots (never claimed
  ones) when over, and do nothing when equal.
- Engine cap for a pool_create call is 1..=32.
"""
from unittest.mock import Mock, call, patch

import pytest

from terra.exceptions import TerraError
from terra.pool import Pool, scale_pool


def _slots(names_claimed):
    """Build a pool_list "pool" payload from (name, claimed) pairs."""
    return [
        {"name": n, "claimed": c, "layers": [], "net": False}
        for n, c in names_claimed
    ]


def _pool(client, size=3):
    """Construct a Pool over a mocked client (init pool_create mocked)."""
    client.pool_create.return_value = {"created": [], "count": 0}
    with patch("terra.pool.TerraClient", return_value=client):
        return Pool(size=size)


@pytest.fixture(autouse=True)
def _cleanup_mocks():
    yield
    patch.stopall()


# ── grow: delta, not total ──────────────────────────────────────────

def test_grow_creates_delta_not_total():
    """grow(2) on a size-3 pool must pool_create(2), not 5 (or 4)."""
    client = Mock()
    pool = _pool(client, size=3)
    client.reset_mock()  # forget the init pool_create(3)

    pool.grow(2)

    client.pool_create.assert_called_once_with(2, kernel=None, net=False)
    assert pool._size == 5


def test_grow_default_count_is_one():
    """grow() with no argument adds exactly one VM."""
    client = Mock()
    pool = _pool(client, size=3)
    client.reset_mock()

    pool.grow()

    client.pool_create.assert_called_once_with(1, kernel=None, net=False)
    assert pool._size == 4


# ── scale: reach exactly `target` idle slots ────────────────────────

def test_scale_grow_creates_idle_delta():
    """scale(5) with 3 idle slots must pool_create(2), not 5."""
    client = Mock()
    client.pool_list.return_value = {
        "pool": _slots([("pool-0", False), ("pool-1", False), ("pool-2", False)]),
        "count": 3,
    }
    pool = _pool(client, size=3)
    client.reset_mock()

    pool.scale(5)

    client.pool_create.assert_called_once_with(2, kernel=None, net=False)
    client.vm_destroy.assert_not_called()
    assert pool._size == 5


def test_scale_shrink_destroys_idle_only():
    """scale(1) with 3 idle + 2 claimed must destroy only idle surplus."""
    client = Mock()
    client.pool_list.return_value = {
        "pool": _slots(
            [
                ("pool-0", False),
                ("pool-1", False),
                ("pool-2", False),
                ("pool-3", True),
                ("pool-4", True),
            ]
        ),
        "count": 5,
    }
    pool = _pool(client, size=3)
    client.reset_mock()
    client.pool_shrink.return_value = {"removed": ["pool-0", "pool-1"], "count": 2}

    pool.scale(1)

    # surplus = 3 idle - 1 target = 2 → engine-side atomic shrink
    assert client.pool_shrink.call_args.args == (2,)
    # claimed slots are never touched by a scale-down (engine guarantees it)
    client.vm_destroy.assert_not_called()
    client.pool_create.assert_not_called()
    assert pool._size == 1


def test_scale_equal_is_noop():
    """scale(3) with exactly 3 idle slots must not create or destroy."""
    client = Mock()
    client.pool_list.return_value = {
        "pool": _slots([("pool-0", False), ("pool-1", False), ("pool-2", False)]),
        "count": 3,
    }
    pool = _pool(client, size=3)
    client.reset_mock()

    pool.scale(3)

    client.pool_create.assert_not_called()
    client.vm_destroy.assert_not_called()
    assert pool._size == 3


def test_scale_rejects_out_of_range():
    """Targets below 1 or above 32 (engine cap) must raise a clear error."""
    client = Mock()
    client.pool_list.return_value = {"pool": [], "count": 0}
    for bad in (0, -1, 33, 100):
        with pytest.raises(TerraError, match="between 1 and 32"):
            scale_pool(client, bad)


# ── module-level helper (shared with the CLI) ───────────────────────

def test_scale_pool_module_level_returns_summary():
    """scale_pool() reports what it created/destroyed for CLI output."""
    client = Mock()
    client.pool_create.return_value = {"created": ["pool-3"], "count": 1}
    client.pool_list.return_value = {
        "pool": _slots([("pool-0", False), ("pool-1", False)]),
        "count": 2,
    }

    out = scale_pool(client, 3)

    client.pool_create.assert_called_once_with(1, kernel=None, net=False)
    assert out == {"idle": 3, "created": ["pool-3"], "destroyed": []}


def test_scale_pool_module_level_shrink_destroys():
    """scale_pool() destroys surplus idle VMs by name, in pool order."""
    client = Mock()
    client.pool_list.return_value = {
        "pool": _slots(
            [
                ("pool-0", False),
                ("pool-1", False),
                ("pool-2", False),
                ("pool-3", True),
            ]
        ),
        "count": 4,
    }

    client.pool_shrink.return_value = {"removed": ["pool-0"], "count": 1}
    out = scale_pool(client, 2)

    # 3 idle - 2 target = 1 surplus → engine-side atomic shrink of 1
    assert client.pool_shrink.call_args.args == (1,)
    assert out["destroyed"] == ["pool-0"]
    client.vm_destroy.assert_not_called()

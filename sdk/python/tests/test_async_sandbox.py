"""Unit tests for AsyncSandbox delegation to the sync Sandbox.

No daemon, no VM — the sync Sandbox is mocked.  Verifies the
async wrapper members that mirror Sandbox surface (vm, tenant,
policy, pool_backed, destroy_tenant) delegate correctly.
"""

from __future__ import annotations

import asyncio
from unittest.mock import Mock

import pytest

from terra.async_sandbox import AsyncSandbox
from terra.sandbox import Sandbox


@pytest.fixture
def sync_mock() -> Mock:
    sb = Mock(spec=Sandbox)
    sb.vm = "tenant-test"
    sb.tenant = "test"
    sb.pool_backed = False
    sb.policy = Mock(return_value={"capabilities": [], "limits": {"memory_mb": 256}})
    return sb


@pytest.fixture
def async_sb(sync_mock: Mock) -> AsyncSandbox:
    sb = AsyncSandbox.__new__(AsyncSandbox)
    sb._sync = sync_mock
    return sb


def test_cheap_properties_delegate(async_sb: AsyncSandbox, sync_mock: Mock):
    assert async_sb.vm == "tenant-test"
    assert async_sb.tenant == "test"
    assert async_sb.pool_backed is False
    assert async_sb.vm == sync_mock.vm
    assert async_sb.tenant == sync_mock.tenant
    assert async_sb.pool_backed == sync_mock.pool_backed


def test_policy_runs_in_executor(async_sb: AsyncSandbox, sync_mock: Mock):
    result = asyncio.run(async_sb.policy())
    assert result == {"capabilities": [], "limits": {"memory_mb": 256}}
    sync_mock.policy.assert_called_once_with()


def test_exec_forwards_background_flag(async_sb: AsyncSandbox, sync_mock: Mock):
    """AsyncSandbox.exec(background=True) reaches the sync exec unchanged."""
    from terra.sessions import Session

    fake_session = Session.__new__(Session)
    sync_mock.exec = Mock(return_value=fake_session)
    session = asyncio.run(async_sb.exec(["sleep", "1"], background=True))
    assert session is fake_session
    sync_mock.exec.assert_called_once_with(
        ["sleep", "1"], cwd=None, env=None, timeout=None, check=False,
        sandboxed=True, policy=None, background=True,
    )


def test_destroy_tenant_delegates(sync_mock: Mock, monkeypatch: pytest.MonkeyPatch):
    called: list[tuple] = []

    def fake_destroy(cls, tenant_id: str):
        called.append(tenant_id)
        return "destroyed"

    monkeypatch.setattr(Sandbox, "destroy_tenant", classmethod(fake_destroy))
    result = asyncio.run(AsyncSandbox.destroy_tenant("research-team"))
    assert called == ["research-team"]
    assert result is None

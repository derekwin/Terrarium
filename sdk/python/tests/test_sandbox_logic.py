"""Sandbox construction logic — daemon and client mocked, no VM."""
from unittest.mock import Mock, patch

import pytest

import terra.sandbox as sb_mod
from terra.client import TerraError as ClientError
from terra.exceptions import TerraError
from terra.sandbox import Sandbox


def _mock_env(existing_records, create_resp):
    client = Mock()
    client.sandbox_list.return_value = {"sandboxes": existing_records, "count": len(existing_records)}
    client.sandbox_create.return_value = create_resp
    # No VM exists by name in this mocked world: the old name-based probe
    # (client.vm_info("tenant-<t>")) must fail, while sandbox_list drives
    # the real decision. Keeps the tests biting (see sanity-check step).
    client.vm_info.side_effect = ClientError("no such vm")
    dm = Mock()
    # Patches are started here (not used as a `with` block) so they stay
    # active while the caller constructs Sandbox() after this helper returns.
    patch.object(sb_mod, "DaemonManager", return_value=dm).start()
    patch.object(sb_mod, "TerraClient", return_value=client).start()
    return client


@pytest.fixture(autouse=True)
def _cleanup_mocks():
    """Undo any patches left started by _mock_env after each test."""
    yield
    patch.stopall()


def test_pool_backed_tenant_reuses_without_template():
    """Second Sandbox of a pool-backed tenant needs no template/layers."""
    client = _mock_env(
        existing_records=[{"id": "sb-aaaabbbb", "vm_name": "pool-0", "pool_backed": True}],
        create_resp={"id": "sb-ccccdddd", "vm": "pool-0", "workdir": "/workdir/sb-ccccdddd", "pool": True},
    )
    sb = Sandbox(tenant="research")  # no template, no layers → must NOT raise
    assert sb.pool_backed is True
    assert sb.vm == "pool-0"
    client.sandbox_create.assert_called_once()
    kwargs = client.sandbox_create.call_args.kwargs
    assert kwargs["pool"] is True


def test_new_tenant_requires_template_or_layers():
    """First Sandbox of a tenant without VM spec must raise."""
    _mock_env(existing_records=[], create_resp={})
    with pytest.raises(TerraError, match="template or layers required"):
        Sandbox(tenant="fresh")


def test_existing_tenant_no_extra_vm_spec_fields():
    """Reuse path must not demand vmspec — and must pass pool=False through."""
    client = _mock_env(
        existing_records=[{"id": "sb-aaaabbbb", "vm_name": "tenant-x", "pool_backed": False}],
        create_resp={"id": "sb-ccccdddd", "vm": "tenant-x", "workdir": "/workdir/sb-ccccdddd", "pool": False},
    )
    sb = Sandbox(tenant="x", pool=False)
    assert sb.pool_backed is False
    assert sb.vm == "tenant-x"
    client.sandbox_create.assert_called_once()
    assert client.sandbox_create.call_args.kwargs["pool"] is False
    assert "kernel" not in client.sandbox_create.call_args.kwargs
    assert "layers" not in client.sandbox_create.call_args.kwargs

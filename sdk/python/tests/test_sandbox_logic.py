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


# ── background exec sessions ──────────────────────────────────────


def _engine_sandbox(client):
    """Construct a Sandbox bound to a mocked engine client (tenant-x)."""
    return Sandbox(tenant="x", pool=False)


def test_exec_background_returns_session_handle():
    """background=True → sandbox_exec with exec_mode="background"; Session handle."""
    client = _mock_env(
        existing_records=[{"id": "sb-ccccdddd", "vm_name": "tenant-x", "pool_backed": False}],
        create_resp={"id": "sb-ccccdddd", "vm": "tenant-x", "workdir": "/workdir/sb-ccccdddd", "pool": False},
    )
    client.sandbox_exec.return_value = {
        "session_id": "ses-abc123", "sandbox": "sb-ccccdddd", "status": "started",
    }
    client.session_status.return_value = {
        "session_id": "ses-abc123", "vm_name": "tenant-x",
        "args": ["sleep", "10"], "status": "running", "exit_code": None,
        "stdout": "", "stderr": "", "sandbox": "sb-ccccdddd",
    }
    client.session_kill.return_value = {"session_id": "ses-abc123", "status": "killed"}

    sb = _engine_sandbox(client)
    session = sb.exec(["sleep", "10"], background=True)

    from terra.sessions import Session
    assert isinstance(session, Session)
    assert session.session_id == "ses-abc123"
    assert session.sandbox_id == "sb-ccccdddd"
    kwargs = client.sandbox_exec.call_args.kwargs
    assert kwargs["exec_mode"] == "background"
    assert client.sandbox_exec.call_args.args[1] == ["sleep", "10"]

    assert session.status()["status"] == "running"
    client.session_status.assert_called_once_with("ses-abc123")
    assert session.kill()["status"] == "killed"
    client.session_kill.assert_called_once_with("ses-abc123")
    assert repr(session) == "Session(id='ses-abc123', sandbox='sb-ccccdddd')"


def test_exec_blocking_sends_no_exec_mode():
    """Byte-identical wire for background=False: no exec_mode key."""
    client = _mock_env(
        existing_records=[{"id": "sb-ccccdddd", "vm_name": "tenant-x", "pool_backed": False}],
        create_resp={"id": "sb-ccccdddd", "vm": "tenant-x", "workdir": "/workdir/sb-ccccdddd", "pool": False},
    )
    client.sandbox_exec.return_value = {"exit_code": 0, "stdout": "ok", "stderr": ""}
    sb = _engine_sandbox(client)
    result = sb.exec(["echo", "hi"])
    assert result.stdout == "ok"
    assert "exec_mode" not in client.sandbox_exec.call_args.kwargs


def test_exec_background_on_pool_claimed_sandbox_raises():
    """Pool-acquire sessions (vm_exec path) cannot do engine-tracked background."""
    client = Mock()
    client.vm_exec.return_value = {"exit_code": 0, "stdout": "", "stderr": ""}
    sb = Sandbox._from_claimed_vm(client, "pool-0", "/workdir/s1", "s1")
    with pytest.raises(TerraError, match="background exec requires an engine sandbox"):
        sb.exec(["sleep", "1"], background=True)
    client.sandbox_exec.assert_not_called()


def test_exec_background_timeout_semantics():
    """Background: no timeout → engine max 3600 (not the 600s sandbox default);
    explicit timeout honored; blocking keeps the sandbox default."""
    client = _mock_env(
        existing_records=[{"id": "sb-ccccdddd", "vm_name": "tenant-x", "pool_backed": False}],
        create_resp={"id": "sb-ccccdddd", "vm": "tenant-x", "workdir": "/workdir/sb-ccccdddd", "pool": False},
    )
    sb = _engine_sandbox(client)

    client.sandbox_exec.return_value = {
        "session_id": "ses-1", "sandbox": "sb-ccccdddd", "status": "started",
    }
    sb.exec(["sleep", "1000"], background=True)
    assert client.sandbox_exec.call_args.args[2] == 3600

    sb.exec(["sleep", "100"], background=True, timeout=120)
    assert client.sandbox_exec.call_args.args[2] == 120

    client.sandbox_exec.return_value = {"exit_code": 0, "stdout": "ok", "stderr": ""}
    sb.exec(["echo", "hi"])
    assert client.sandbox_exec.call_args.args[2] == 600

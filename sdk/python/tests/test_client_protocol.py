"""TerraClient protocol construction — pure logic, no daemon."""
from unittest.mock import Mock, patch

import pytest

from terra.client import TerraClient


@pytest.fixture
def client():
    c = TerraClient()
    c._connect = Mock(return_value=None)  # never actually connects
    return c


def _captured(mock_send):
    assert mock_send.call_count == 1
    return mock_send.call_args.args[0]


def test_sandbox_create_carries_full_spec(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        policy = {
            "capabilities": [
                {"File": {"path": {"Prefix": "/opt"}, "access": "Read"}}
            ],
            "limits": {"memory_mb": 256, "procs": 20},
        }
        client.sandbox_create(
            "team", policy=policy, pool=True,
            kernel="/k", initramfs="/i", layers=["base"], cpus=1,
            max_cpus=16, memory_mb=256, net=False,
        )
    cmd = _captured(m)
    assert cmd["command"] == "sandbox_create"
    assert cmd["tenant"] == "team"
    assert cmd["layers"] == ["base"]
    assert cmd["kernel"] == "/k"
    assert cmd["initramfs"] == "/i"
    assert cmd["cpus"] == 1
    assert cmd["max_cpus"] == 16
    assert cmd["memory_mb"] == 256
    assert cmd["net"] is False
    assert cmd["policy"] == policy  # validate_policy passthrough
    assert "pool" not in cmd  # default (True) → omitted


def test_sandbox_create_pool_false_is_explicit(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_create("team", pool=False, layers=["base"])
    cmd = _captured(m)
    assert cmd["pool"] is False


def test_vm_exec_sandbox_flag_only_when_true(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.vm_exec("vm", ["echo", "hi"])
    cmd = _captured(m)
    assert "sandbox" not in cmd  # default: unsandboxed VM exec, omit flag

    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.vm_exec("vm", ["echo", "hi"], sandbox=True)
    cmd = _captured(m)
    assert cmd["sandbox"] is True


def test_sandbox_exec_defaults_sandbox_omitted(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_exec("sb-1", ["echo", "hi"])
    cmd = _captured(m)
    assert "sandbox" not in cmd  # engine default (True) applies

    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_exec("sb-1", ["echo", "hi"], sandbox=False)
    cmd = _captured(m)
    assert cmd["sandbox"] is False


def test_pool_claim_layers_always_sent(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.pool_claim(["base"])
    cmd = _captured(m)
    assert cmd["layers"] == ["base"]


def test_sandbox_exec_background_mode(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_exec("sb-1", ["sleep", "10"], exec_mode="background")
    cmd = _captured(m)
    assert cmd["exec_mode"] == "background"

    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_exec("sb-1", ["sleep", "10"])
    cmd = _captured(m)
    assert "exec_mode" not in cmd  # blocking default → omitted


def test_session_status_command(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.session_status("ses-1")
    cmd = _captured(m)
    assert cmd == {"command": "session_status", "session_id": "ses-1"}


def test_session_kill_command(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.session_kill("ses-1")
    cmd = _captured(m)
    assert cmd == {"command": "session_kill", "session_id": "ses-1"}


def test_session_list_command(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.session_list()
    cmd = _captured(m)
    assert cmd == {"command": "session_list"}


def test_terra_error_is_unified_class():
    """client.TerraError must BE exceptions.TerraError — single source of truth."""
    from terra.client import TerraError as ClientTerraError
    from terra.exceptions import TerraError

    assert ClientTerraError is TerraError

"""FilesClient protocol construction — mocked exec, no daemon/VM."""
from unittest.mock import Mock

import pytest

from terra.sandbox import FilesClient


@pytest.fixture
def sb():
    mock = Mock()
    return mock


def _exec_result(stdout="", exit_code=0):
    r = Mock()
    r.stdout = stdout
    r.exit_code = exit_code
    r.stderr = ""
    return r


def test_download_uses_base64_channel(sb):
    """download must base64-encode in the guest (binary-safe), not raw cat."""
    import base64

    payload = bytes(range(256))
    sb.exec.return_value = _exec_result(stdout=base64.b64encode(payload).decode())
    fc = FilesClient(sb)
    fc.download("bin.dat", "/tmp/x_dl_test.bin")
    # guest command is `base64 < path`
    args = sb.exec.call_args.args[0]
    assert args[0] == "sh" and "-c" in args
    cmd = args[2]
    assert cmd.startswith("base64 < ") and "bin.dat" in cmd
    got = open("/tmp/x_dl_test.bin", "rb").read()
    assert got == payload


def test_read_uses_raw_cat(sb):
    """read stays a text API (raw cat); binary must go through download."""
    sb.exec.return_value = _exec_result(stdout="hello")
    fc = FilesClient(sb)
    assert fc.read("a.txt") == "hello"
    assert sb.exec.call_args.args[0] == ["cat", "a.txt"]

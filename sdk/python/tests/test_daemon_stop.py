"""daemon_stop outcome classification — mocked transport, no daemon."""
from unittest.mock import patch

import pytest

from terra.client import TerraClient, TerraError
from terra.daemon import EMBEDDED_STOP_REFUSAL, DaemonError, daemon_stop


def test_acknowledged_stop_returns_stopped():
    """A daemon that answers daemon_stop is shutting down."""
    with patch.object(
        TerraClient,
        "_send",
        return_value={"status": "ok", "data": {"message": "daemon stopping"}},
    ):
        assert daemon_stop("/tmp/terra.sock") == "stopped"


def test_embedded_refusal_returns_refused():
    """An embedded daemon refuses daemon_stop by design — no-op success."""
    with patch.object(TerraClient, "_send", side_effect=TerraError(EMBEDDED_STOP_REFUSAL)):
        assert daemon_stop("/tmp/terra.sock") == "refused"


def test_unreachable_daemon_returns_gone():
    """An unreachable daemon is already gone."""
    with patch.object(TerraClient, "_send", side_effect=ConnectionRefusedError()):
        assert daemon_stop("/tmp/terra.sock") == "gone"


def test_other_engine_error_raises_daemon_error():
    """Any other engine error surfaces as DaemonError."""
    with patch.object(TerraClient, "_send", side_effect=TerraError("some other failure")):
        with pytest.raises(DaemonError, match="some other failure"):
            daemon_stop("/tmp/terra.sock")

"""paths.root() resolution — service-data default (/var/lib/terra),
user fallback, TERRA_HOME override (docker-style data directory)."""
import pathlib
from unittest.mock import patch

import terra.paths as P


def test_root_process_uses_system_data_dir():
    # root (daemon) → /var/lib/terra regardless of writability
    with (
        patch.object(P.os, "geteuid", return_value=0),
        patch.object(P.os, "environ", {}),
        patch.object(P, "_ROOT", None),
    ):
        assert str(P._default_root()) == "/var/lib/terra"


def test_non_root_with_writable_system_dir_uses_it():
    with (
        patch.object(P.os, "geteuid", return_value=1001),
        patch.object(P.os, "environ", {"HOME": "/home/dev"}),
        patch.object(P, "_writable", return_value=True),
        patch.object(P, "_ROOT", None),
    ):
        assert str(P._default_root()) == "/var/lib/terra"


def test_non_root_without_system_install_falls_back_to_user():
    with (
        patch.object(P.os, "geteuid", return_value=1001),
        patch.object(P.os, "environ", {"HOME": "/home/dev"}),
        patch.object(P, "_writable", return_value=False),
        patch.object(P, "_ROOT", None),
        patch("pathlib.Path.home", return_value=pathlib.Path("/home/dev")),
    ):
        assert str(P._default_root()) == "/home/dev/.local/share/terra"


def test_terra_home_overrides_everything(tmp_path):
    with (
        patch.object(P.os, "geteuid", return_value=0),
        patch.object(P.os, "environ", {"TERRA_HOME": str(tmp_path)}),
        patch.object(P, "_ROOT", None),
    ):
        assert P.root() == tmp_path

"""Terrarium Engine Python SDK.

Quickstart (zero setup — assets and daemon are managed automatically):

    from terra.daemon import Daemon
    from terra.client import TerraClient
    from terra.vm import create

    with Daemon():
        client = TerraClient()
        client.pool_create(1)
        claim = client.pool_claim(["base"])
        print(client.vm_exec(claim["name"], ["ls", "/newroot"]))
"""

from .client import TerraClient, TerraError
from . import vm
from . import paths
from . import assets
from . import images
from . import daemon

__all__ = [
    "TerraClient",
    "TerraError",
    "vm",
    "paths",
    "assets",
    "images",
    "daemon",
]

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
from .config import HostConfig
from . import direct
from .direct import configure, create, list_vms, connect
from . import sandbox
from .sandbox import Sandbox
from . import sessions
from .sessions import Session
from . import async_sandbox
from .async_sandbox import AsyncSandbox
from . import pool
from .pool import Pool
from .daemon import session

__all__ = [
    "TerraClient",
    "TerraError",
    "vm",
    "paths",
    "assets",
    "images",
    "daemon",
    "session",
    "HostConfig",
    "direct",
    "sandbox",
    "Sandbox",
    "sessions",
    "Session",
    "async_sandbox",
    "AsyncSandbox",
    "pool",
    "Pool",
    "create",
    "connect",
    "list_vms",
    "configure",
]

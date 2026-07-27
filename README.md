# Terrarium Engine

**Production-grade agent sandboxing — deploy secure, isolated execution environments with high single-node density.**

Terrarium Engine is a scheduling and control layer that decouples agent sandboxing from specific VM and sandbox technologies. Think of it as the control plane that turns any Linux host into a multi-tenant agent runtime — with hardware-level VM isolation, pluggable sandbox backends, and a layered virtiofs filesystem (EROFS + OverlayFS, see docs/plans).

## Why Terrarium

Running AI agents in production means running untrusted code at scale. Containers share a kernel. MicroVMs are slow to provision. Terrarium sits in the sweet spot:

| | Container | VM-only | Terrarium |
|---|---|---|---|
| Isolation | Weak (shared kernel) | Strong (KVM) | **Strong (KVM + sandbox)** |
| Density | High | Low | **High** |
| Provisioning | Fast | Slow (~1s) | **Fast (~1s via CH)** |
| File persistence | Ephemeral | Disk image | **Layered virtiofs (shared page cache)** |
| Resource control | cgroup | VM config | **Network QoS via tc** |
| Sandbox backends | N/A | N/A | **Pluggable (Sandlock, OpenShell)** |

## Architecture

```
┌─ Host ──────────────────────────────────────────────────────┐
│                                                              │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                Terrarium Engine                       │   │
│  │     daemon · CLI · Python SDK · MCP Server            │   │
│  └──────────────────────┬───────────────────────────────┘   │
│                         │                                    │
│          ┌──────────────┴──────────────┐                     │
│          ▼                             ▼                     │
│  ┌───────────────┐            ┌────────────────┐            │
│  │  CH Adapter   │            │ SandboxAdapter  │            │
│  │  (VmAdapter)  │            │ Sandlock/OpenShell│          │
│  └───────┬───────┘            └───────┬────────┘            │
│          │                            │                      │
│          ▼                            ▼                      │
│  ┌─────────────────────────────────────────────┐            │
│  │          Cloud Hypervisor VM × N            │            │
│  │  ┌───────────────────────────────────────┐  │            │
│  │  │  guest-proxy ← host→guest relay       │  │            │
│  │  │  sandlock CLI / openshell CLI         │  │            │
│  │  │  Agent process ◄── Sandbox isolation  │  │            │
│  │  └───────────────────────────────────────┘  │            │
│  │  per VM: virtiofs rootfs (EROFS layers + private upperdir)   │            │
│  └─────────────────────────────────────────────┘            │
│                                                              │
│  Multi-backend via Adapter traits                              │
└──────────────────────────────────────────────────────────────┘
```

**Two adapter layers, trait-based:**

| Trait | What it does | Implementations |
|---|---|---|
| `VmAdapter` | Spawn, resize, snapshot VMs | Cloud Hypervisor (Firecracker dropped — no virtiofs) |
| `SandboxAdapter` | Create, exec, destroy sandboxes | Sandlock, OpenShell |

## Key Capabilities

### Adapter Trait Architecture

- Engine completely decoupled from VM implementations via `VmAdapter` / `VmHandle` traits
- Pluggable backends: Cloud Hypervisor, Sandlock, OpenShell (Firecracker removed — no virtiofs support)
- Unified error type (`AdapterError`) across all adapters
- Async runtime (tokio) for concurrent VM operations

### Layered Filesystem (virtiofs)

```
  upperdir   (writable, per-VM host dir)      <- user data
  tool layers (read-only EROFS, composable)   <- tools/runtime
  base layer  (read-only EROFS, shared)       <- system
```
Read-only layers are star-composed on the host with OverlayFS (arbitrary
combinations, shared page cache) and exposed to the VM via virtiofs with
`cache=always` (note: DAX was removed from Cloud Hypervisor — see
docs/fs-m2-benchmark.md). Writes copy-up into the per-VM upperdir.
Compute and data lifecycles are separate: VM commands never delete data.
Design: docs/plans.

### Pluggable Sandbox Backends

| Backend | Isolation | Unique |
|---|---|---|
| **Sandlock** | Landlock + seccomp-bpf + seccomp notif | No root needed, COW FS, HTTP ACL, ~5ms startup |
| **OpenShell** (NVIDIA) | Container + Landlock + OPA proxy | Inference routing, credential injection, GPU |
| **guest-proxy** | Thin relay | Host↔guest command forwarding |

### Resource Control

- Network QoS: per-VM egress/ingress rate limiting via Linux tc
- Dynamic resize: CPU, memory online adjustment without reboot
- Exec timeout + output cap: all sandbox commands limited to 60s + 10MB output

## Quick Start (Python SDK / MCP)

Install the SDK:

```bash
pip install -e sdk/python
```

**The only concept you need is `layers`**: names of environment layers,
e.g. `["python312", "base"]` (tool layers first, the base system last).
Everything else — daemon, binaries, directories — is automatic.

### Mode A: single user, fully managed by the SDK

Zero setup — the SDK resolves the engine, binaries, and directories,
and cleans up when done:

```python
from terra.daemon import Daemon
from terra.client import TerraClient

with Daemon():
    c = TerraClient()

    # Grab a VM from the warm pool (layers are hot-plugged)
    claim = c.pool_claim(["python312", "base"])
    name = claim["name"]

    # Run commands inside the VM
    print(c.vm_exec(name, ["python3", "-c", "import numpy; print(numpy.__version__)"]))

    # Return it to the pool
    c.pool_release(name)
```

### Mode B: client against an existing server daemon

When an admin already runs the daemon on a server, you just connect:

```python
from terra.client import TerraClient
from terra.vm import create

c = TerraClient()          # default socket; remote: TerraClient("/path/forwarded.sock")

# Create a VM with an environment
vm = create("dev", "target/guest/vmlinux.bin",
            initramfs="target/guest/initramfs-virtiofs.cpio.gz",
            layers=["python312", "base"], cpus=2, memory_mb=512, net=True)

print(vm.info())                                # state / cpus / memory_mb
print(vm.exec(["python3", "--version"]))        # run inside the VM
vm.resize(cpus=4)                               # scale up online
vm.destroy()

# Or use the warm pool (faster path)
claim = c.pool_claim(["python312", "base"])
print(c.vm_exec(claim["name"], ["python3", "-c", "print(2**10)"]))
c.pool_release(claim["name"])
```

API cheat sheet (`TerraClient` / `Vm`):

| Area | Methods |
|---|---|
| VM | `vm_create / vm_list / vm_info / vm_resize / vm_shutdown / vm_kill / vm_destroy` |
| Exec | `vm_exec(name, args, timeout_secs=60)` |
| Pool | `pool_claim / pool_list / pool_release` |

### MCP (for AI agents)

The MCP server runs over stdio — point your agent (Claude Code / Desktop, etc.) at it:

```json
{
  "mcpServers": {
    "terrarium": {
      "command": "/path/to/target/release/terra-mcp",
      "env": { "TERRA_SOCKET": "/tmp/terra.sock" }
    }
  }
}
```

User-surface tools visible to the agent: `terra_vm_create/list/info/resize/shutdown/kill/destroy`, `terra_exec`, `terra_pool_claim/list/release`, `terra_attach_fs/detach_fs`. Canonical flow:

```
terra_pool_claim(layers=["python312","base"])
  → terra_exec(name, args=["python3","-c","print(2**10)"])
  → terra_pool_release(name)
```

> Admin operations (daemon lifecycle, image building, network teardown, pool creation) are out of the user surface — see the `terra` CLI and `AGENTS.md`.

## Repository

```
crates/
├── engine/          Engine daemon + CLI + VM lifecycle
├── adapter/
│   ├── traits/      VmAdapter + SandboxAdapter trait definitions
│   ├── cloud-hypervisor/  CH adapter (tokio async client)
│   ├── sandlock/    Sandlock adapter (SandboxAdapter, capability-gated)
│   └── openshell/   OpenShell adapter (SandboxAdapter)
├── protocol/        Shared Command/Response types (JSON protocol)
├── guest-proxy/     Host↔guest command relay daemon
├── network/         Per-VM tc-based network QoS
├── cli/             terra CLI (uses protocol crate)
└── mcp/             MCP Server (stdio JSON-RPC)

sdk/python/          Python SDK

thirdparty/          Third-party deps + patch registry
images/              Guest kernel + rootfs build
```

## Roadmap

- **M0** ✅ CH base, guest images, baseline measurements
- **M1** ✅ Engine daemon, CLI, VM lifecycle
- **M2** ✅ Adapter layer, Sandlock/OpenShell, async tokio runtime, Python SDK
- **M3** ✅ virtiofs filesystem (EROFS layers + OverlayFS), warm pool, networking (tap/NAT/DHCP), layer build-by-doing
- **M4** 🔲 Pool auto-scaling, snapshot fault tolerance, density benchmarks, observability

## License

Apache 2.0. Built on Cloud Hypervisor and Linux kernel features. See `THIRD-PARTY` for acknowledgments.

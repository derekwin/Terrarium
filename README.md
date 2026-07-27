# Terrarium Engine

**Production-grade agent sandboxing — deploy secure, isolated execution environments with high single-node density.**

Terrarium Engine is a scheduling and control layer for agent runtime execution environments, decoupled from specific VM and sandbox technologies — hardware-level VM isolation, pluggable sandbox backends, and a layered virtiofs filesystem.

## Why Terrarium

Running AI agents in production means running untrusted code at scale. Containers share a kernel. MicroVMs are slow to provision. Terrarium sits in the sweet spot:

| | Container | VM-only | Terrarium |
|---|---|---|---|
| Isolation | Weak (shared kernel) | Strong (KVM) | **Strong (KVM + sandbox)** |
| Density | High | Low | **High** |
| Provisioning | Fast | Slow (~1s) | **Fast (warm-pool quick start)** |
| File persistence | Ephemeral | Disk image | **Layered virtiofs** |
| Resource control | cgroup | VM config | **tc QoS** |
| Sandbox backends | N/A | N/A | **Pluggable (Sandlock, OpenShell)** |

## Architecture

```
┌─ Host ──────────────────────────────────────────────────────┐
│  ┌──────────────────────────────────────────────────────┐   │
│  │                Terrarium Engine                       │   │
│  │     daemon · CLI · Python SDK · MCP Server            │   │
│  └──────────────────────┬───────────────────────────────┘   │
│          ┌──────────────┴──────────────┐                     │
│          ▼                             ▼                     │
│  ┌───────────────┐            ┌────────────────┐            │
│  │  CH Adapter   │            │ SandboxAdapter  │            │
│  │  (VmAdapter)  │            │ Sandlock/OpenShell│          │
│  └───────┬───────┘            └───────┬────────┘            │
│          ▼                            ▼                      │
│  ┌─────────────────────────────────────────────┐            │
│  │          Cloud Hypervisor VM × N            │            │
│  │  ┌───────────────────────────────────────┐  │            │
│  │  │  guest-proxy ← host→guest relay       │  │            │
│  │  │  sandlock CLI / openshell CLI         │  │            │
│  │  │  Agent process ◄── Sandbox isolation  │  │            │
│  │  └───────────────────────────────────────┘  │            │
│  │  per VM: virtiofs rootfs (EROFS layers + private upperdir) ││
│  └─────────────────────────────────────────────┘            │
│  Multi-backend via Adapter traits                            │
└──────────────────────────────────────────────────────────────┘
```

**Two adapter layers, trait-based:**

| Trait | What it does | Implementations |
|---|---|---|
| `VmAdapter` | Spawn, resize, snapshot VMs | Cloud Hypervisor |
| `SandboxAdapter` | Create, exec, destroy sandboxes | Sandlock, OpenShell |

## Key Capabilities

### Adapter Trait Architecture

- Engine completely decoupled from VM implementations via `VmAdapter` / `VmHandle` traits
- Pluggable backends: Cloud Hypervisor, Sandlock, OpenShell
- Unified error type (`AdapterError`) across all adapters
- Async runtime (tokio) for concurrent VM operations

### Layered Filesystem (virtiofs)

```
  upperdir   (writable, per-VM host dir)      <- user data
  tool layers (read-only EROFS, composable)   <- tools/runtime
  base layer  (read-only EROFS, shared)       <- system
```

Read-only layers are star-composed on the host with OverlayFS (arbitrary combinations, shared page cache) and exposed to the VM via virtiofs with `cache=always`. Writes copy-up into the per-VM upperdir. Compute and data lifecycles are separate: VM commands never delete data.

### Pluggable Sandbox Backends

| Backend | Isolation | Unique |
|---|---|---|
| **Sandlock** | Landlock + seccomp-bpf + seccomp notif | No root needed, COW FS, HTTP ACL, ~5ms startup (requires host Landlock ABI ≥ v5) |
| **OpenShell** (NVIDIA) | Container + Landlock + OPA proxy | Inference routing, credential injection, GPU |
| **guest-proxy** | Relay | Host↔guest command forwarding (vsock) |

### Resource Control

- Network QoS: per-VM egress/ingress rate limiting and priority (Linux tc)
- Dynamic resize: CPU and memory online adjustment without reboot (verified: 100% effective for both)
- Networking: tap + host NAT + dnsmasq DHCP — ready with `create --net`, managed via `net-list` / `net-down`
- Exec timeout + output cap: 60s default per command (up to 3600s) + 10MB output cap

## Quick Start — Three Ways to Use Terrarium

### 1. `terra` CLI — the admin tool (docker-style)

For host administrators: manage the daemon, images, network, pools,
and inspect everything.

```bash
# daemon (local use doesn't need root; networking does)
target/release/engine daemon

# remote-capable daemon (token-gated TCP)
TERRA_TOKEN=secret target/release/engine daemon --tcp 0.0.0.0:19099

terra image kernel --version 6.12     # build the guest kernel
terra image layer-build python312 --script setup.sh   # build a tool
                                        # layer by configuring inside a
                                        # builder VM (env is proven)
terra image layers                    # list available layers
terra pool-create --size 3            # warm pool
terra create dev --kernel ... --initramfs ... --layers python312,base --net
terra list / info dev / resize dev --cpus 4
terra net-list / net-down             # networking
terra destroy dev
```

### 2. Python direct mode — throwaway VMs, nothing to manage

Zero setup and zero concepts: no daemon, no session, no pool to think
about. The SDK lazily starts a managed engine on first use and cleans
it up at process exit. For scripts, notebooks, and local agents.

```bash
pip install -e sdk/python
```

```python
import terra

vm = terra.create(layers=["python312", "base"])   # that's it
print(vm.exec(["python3", "-c", "import numpy; print(numpy.__version__)"]))
vm.destroy()
```

Want control without writing daemon code? Configure the host once —
images, layers, pool size, VM defaults, token — and your Python script
*is* the daemon program:

```python
from terra import HostConfig, create

terra.configure(HostConfig(kernel="~/img/vmlinux.bin",
                           layer_dir="~/layers",
                           pool_size=4,
                           default_net=True))
vm = create(layers=["python312", "base"])
```

### 3. Client–server mode — use a remote daemon's VM pool

An admin runs the daemon on a server; you connect and use it.

Server (admin):

```bash
sudo env TERRA_TOKEN=secret target/release/engine daemon --tcp 0.0.0.0:19099
```

Client (you):

```python
from terra.client import TerraClient

c = TerraClient("tcp://server-ip:19099", token="secret")
print(c.vm_list())

claim = c.pool_claim(["python312", "base"])      # use the server's pool
print(c.vm_exec(claim["name"], ["python3", "--version"]))
c.pool_release(claim["name"])
```

Or with the CLI: `TERRA_TOKEN=secret terra --socket tcp://server-ip:19099 list`

> TCP is plaintext with a shared token as basic access control — use it
> on trusted networks only. For untrusted networks, tunnel the unix
> socket over SSH instead: `ssh -N -L /tmp/terra.sock:/tmp/terra.sock user@server`.

> Admin operations (daemon lifecycle, image building, network teardown,
> pool creation) live in the `terra` CLI. MCP (agent integration) is
> unchanged.

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

User-surface tools: `terra_vm_create/list/info/resize/shutdown/kill/destroy`,
`terra_exec`, `terra_pool_claim/list/release`, `terra_attach_fs/detach_fs`.
Canonical flow:

```
terra_pool_claim(layers=["python312","base"])
  → terra_exec(name, args=["python3","-c","print(2**10)"])
  → terra_pool_release(name)
```

## Repository

```
crates/
├── engine/          Engine daemon + VM lifecycle + pool management
├── adapter/
│   ├── traits/      VmAdapter + SandboxAdapter trait definitions
│   ├── cloud-hypervisor/  CH adapter (async client + virtiofs composition)
│   ├── sandlock/    Sandlock adapter (capability-gated)
│   └── openshell/   OpenShell adapter
├── protocol/        Shared Command/Response types (JSON protocol)
├── guest-proxy/     Host↔guest command relay (vsock + unix socket)
├── network/         tap/NAT/DHCP + tc QoS
├── cli/             terra CLI (incl. image build commands)
└── mcp/             MCP Server (stdio JSON-RPC)

sdk/python/          Python SDK (zero-config management: daemon/assets/images/paths)

thirdparty/          Third-party deps + patch registry
images/              Guest kernel + rootfs + initramfs build scripts
```

## Roadmap

- **M0** ✅ CH base, guest images, baseline measurements
- **M1** ✅ Engine daemon, CLI, VM lifecycle
- **M2** ✅ Adapter layer, async runtime, Python SDK
- **M3** ✅ virtiofs filesystem (EROFS layers + OverlayFS), warm pool, networking (tap/NAT/DHCP), layer build-by-doing
- **M4** 🔲 Pool auto-scaling, snapshot fault tolerance, density benchmarks, observability

## License

Apache-2.0. Built on Cloud Hypervisor and Linux kernel features. See `THIRD-PARTY` for acknowledgments.

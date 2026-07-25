# Terrarium Engine

**Production-grade agent sandboxing — deploy secure, isolated execution environments with high single-node density.**

Terrarium Engine is a scheduling and control layer that decouples agent sandboxing from specific VM and sandbox technologies. Think of it as the control plane that turns any Linux host into a multi-tenant agent runtime — with hardware-level VM isolation, pluggable sandbox backends, and qcow2 overlay filesystem.

## Why Terrarium

Running AI agents in production means running untrusted code at scale. Containers share a kernel. MicroVMs are slow to provision. Terrarium sits in the sweet spot:

| | Container | VM-only | Terrarium |
|---|---|---|---|
| Isolation | Weak (shared kernel) | Strong (KVM) | **Strong (KVM + sandbox)** |
| Density | High | Low | **High** |
| Provisioning | Fast | Slow (~1s) | **Fast (~1s via CH)** |
| File persistence | Ephemeral | Disk image | **Portable qcow2 overlay** |
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
│  │  每 VM: 独立 qcow2 overlay · network QoS    │            │
│  └─────────────────────────────────────────────┘            │
│                                                              │
│  Adapter trait 解耦，支持多后端                                │
└──────────────────────────────────────────────────────────────┘
```

**Two adapter layers, trait-based:**

| Trait | What it does | Implementations |
|---|---|---|
| `VmAdapter` | Spawn, resize, snapshot VMs | Cloud Hypervisor, Firecracker |
| `SandboxAdapter` | Create, exec, destroy sandboxes | Sandlock, OpenShell |

## Key Capabilities

### Adapter Trait Architecture

- Engine completely decoupled from VM implementations via `VmAdapter` / `VmHandle` traits
- Pluggable backends: Cloud Hypervisor, Firecracker, Sandlock, OpenShell
- Unified error type (`AdapterError`) across all adapters
- Async runtime (tokio) for concurrent VM operations

### Qcow2 Overlay Filesystem

```
  user-data.qcow2    (读写，per-user，可迁移)     ← 用户数据层
  rootfs.qcow2        (只读，共享)                  ← 系统层
```
qcow2 backing chain：写操作落到用户层，读操作逐层回退。可 `scp` 用户层到任意机器，继续工作。

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

## Quick Start

```bash
# Install CH (official release binary)
wget https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/cloud-hypervisor-static
chmod +x cloud-hypervisor-static

# Build guest image
cd images && bash build.sh

# Start engine daemon (env vars for configuration)
export TERRA_CH_BINARY=/usr/local/bin/cloud-hypervisor
export TERRA_STATE_DIR=/var/lib/terra/vms
cargo run -p engine --release -- daemon

# Create VM with overlay
terra create agent-1 --kernel target/guest/vmlinux.bin --rootfs-disk /data/full.qcow2

# List VMs
terra list
```

### Python SDK

```python
import terra

# Create VM
vm = terra.vm.create("agent-1", kernel="target/guest/vmlinux.bin")

# Query VM info
info = vm.info()
print(info["state"])  # "Running"

# Resize VM
vm.resize(cpus=4)

# Clean shutdown
vm.shutdown()
```

## Repository

```
crates/
├── engine/          Engine daemon + CLI + VM lifecycle
├── adapter/
│   ├── traits/      VmAdapter + SandboxAdapter trait definitions
│   ├── cloud-hypervisor/  CH adapter (tokio async client)
│   ├── firecracker/       FC adapter (sync client)
│   ├── sandlock/    Sandlock adapter (SandboxAdapter)
│   └── openshell/   OpenShell adapter (SandboxAdapter)
├── protocol/        Shared Command/Response types (JSON protocol)
├── guest-proxy/     Host↔guest command relay daemon
├── overlay/         Qcow2 overlay filesystem management
├── network/         Per-VM tc-based network QoS
├── cli/             terra CLI (uses protocol crate)
└── mcp/             MCP Server (stdio JSON-RPC)

sdk/python/          Python SDK

thirdparty/          Third-party deps + patch registry
images/              Guest kernel + rootfs build
```

## Roadmap

- **M0** ✅ CH base, guest images, baseline measurements
- **M1** ✅ Engine daemon, CLI, VM lifecycle, qcow2 overlay
- **M2** ✅ Adapter layer, Sandlock/OpenShell, async tokio runtime, Python SDK
- **M3** 🔲 Warm pool, scheduler re-design, multi-node placement, observability
- **M4** 🔲 Snapshot fault tolerance, density benchmarks, full production hardening

## License

Apache 2.0. Built on Cloud Hypervisor and Linux kernel features. See `THIRD-PARTY` for acknowledgments.

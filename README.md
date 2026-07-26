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
│  │  每 VM: virtiofs rootfs（EROFS 层 + 独立 upperdir）· QoS    │            │
│  └─────────────────────────────────────────────┘            │
│                                                              │
│  Adapter trait 解耦，支持多后端                                │
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
  upperdir (可写，per-VM 宿主目录)     ← 用户数据层
  tool layers (EROFS，只读，按需组合)  ← 工具层
  base layer  (EROFS，只读，共享)      ← 系统层
```
宿主侧 OverlayFS 星型组合只读层（任意搭配、page cache 共享），经 virtiofs + DAX 暴露给 VM；写操作 copy-up 进独立 upperdir。计算与数据生命周期分离：VM 命令永不删除数据。设计见 docs/plans。

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

# Create VM (initramfs-based; layered rootfs lands with the virtiofs backend)
terra create agent-1 --kernel target/guest/vmlinux.bin --initramfs target/guest/alpine.cpio

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
- **M3** 🔲 virtiofs filesystem (EROFS layers + OverlayFS + DAX), warm pool, observability
- **M4** 🔲 Snapshot fault tolerance, density benchmarks, full production hardening

## License

Apache 2.0. Built on Cloud Hypervisor and Linux kernel features. See `THIRD-PARTY` for acknowledgments.

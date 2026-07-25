# Terrarium Engine

**Production-grade agent sandboxing — deploy secure, isolated execution environments with high single-node density.**

Terrarium Engine is a scheduling and control layer that decouples agent sandboxing from specific VM and sandbox technologies. Think of it as the control plane that turns any Linux host into a multi-tenant agent runtime — with hardware-level VM isolation, pluggable sandbox backends, and sub-second warm pool provisioning.

## Why Terrarium

Running AI agents in production means running untrusted code at scale. Containers share a kernel. MicroVMs are slow to provision. Terrarium sits in the sweet spot:

| | Container | VM-only | Terrarium |
|---|---|---|---|
| Isolation | Weak (shared kernel) | Strong (KVM) | **Strong (KVM + sandbox)** |
| Density | High | Low | **High** |
| Provisioning | Fast | Slow (~1s) | **Fast (~100ms warm pool)** |
| File persistence | Ephemeral | Disk image | **Portable qcow2 overlay** |
| Resource control | cgroup | VM config | **cgroup v2 + network QoS** |
| Sandbox backends | N/A | N/A | **Pluggable (Sandlock, OpenShell)** |

## Architecture

```
┌─ Host ──────────────────────────────────────────────────────┐
│                                                              │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                Terrarium Engine                       │   │
│  │     scheduler · warm pool · cgroup · placement        │   │
│  │     daemon · CLI · Python SDK · file API              │   │
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
│  │  │  guest-agent ← host→guest relay       │  │            │
│  │  │  sandlock CLI / openshell CLI         │  │            │
│  │  │  Agent process ◄── Sandbox isolation  │  │            │
│  │  └───────────────────────────────────────┘  │            │
│  │  每 VM: 独立 qcow2 overlay · cgroup · QoS   │            │
│  └─────────────────────────────────────────────┘            │
│                                                              │
│  单机 100+ VM · 预热池 ~100ms 启动 · 资源动态扩缩            │
└──────────────────────────────────────────────────────────────┘
```

**Two adapter layers, trait-based:**

| Trait | What it does | Implementations |
|---|---|---|
| `VmAdapter` | Spawn, resize, snapshot VMs | Cloud Hypervisor (Firecracker, K8s Pod planned) |
| `SandboxAdapter` | Create, exec, destroy sandboxes | Sandlock, OpenShell |

## Key Capabilities

### High-Density Scheduling

- Single node: 100+ VMs with network QoS per TAP
- Admission control: pre-check resource availability
- Idle reclamation: scale down unused VMs automatically
- Warm pool: pre-booted VMs ready in ~100ms via snapshot restore

### Three-Layer Filesystem

```
  user-data.qcow2    (读写，per-user，可迁移)     ← 用户数据层
  tools.qcow2         (只读，共享，可组合)          ← 工具层
  rootfs.qcow2        (只读，共享)                  ← 系统层
```
qcow2 backing chain：写操作落到用户层，读操作逐层回退。可 `scp` 用户层到任意机器，继续工作。

### Pluggable Sandbox Backends

| Backend | Isolation | Unique |
|---|---|---|
| **Sandlock** | Landlock + seccomp-bpf + seccomp notif | No root needed, COW FS, HTTP ACL, ~5ms startup |
| **OpenShell** (NVIDIA) | Container + Landlock + OPA proxy | Inference routing, credential injection, GPU |
| **guest-agent** | Thin relay | Host↔guest command forwarding |

### Resource Control

- cgroup v2: per-sandbox memory.max + cpu.weight (kernel-enforced, near-zero overhead)
- Network QoS: per-VM egress/ingress rate limiting via Linux tc
- Dynamic resize: CPU, memory online adjustment without reboot

## Quick Start

```bash
# Install CH (official release binary)
wget https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/cloud-hypervisor-static
chmod +x cloud-hypervisor-static

# Build guest image
cd images && bash build.sh

# Start engine daemon
cargo run -p engine --release -- daemon

# Create VM with overlay
terra vm create agent-1 \
  --kernel target/guest/vmlinux.bin \
  --rootfs-disk /data/base.qcow2 \
  --toolfs-disk /data/tool-python.qcow2

# Run agent in sandbox
terra sandbox exec python3 -c "print(2 ** 10)"
```

### Python SDK

```python
import terra

# Create VM
vm = terra.vm.create("agent-1", kernel="target/guest/vmlinux.bin")

# Execute in sandbox
sb = terra.sandbox.create("agent-1", tools=["python"])
result = sb.exec("python3", "-c", "print(2 ** 10)")
print(result.stdout)  # 1024

# Read agent output
content = sb.read_file("/home/agent/output.txt")
```

## Repository

```
crates/
├── engine/          Scheduler, pool, cgroup, daemon
├── adapter/
│   ├── traits/      VmAdapter + SandboxAdapter
│   ├── ch/          Cloud Hypervisor (self-contained)
│   ├── sandlock/    Sandlock adapter
│   └── openshell/   OpenShell adapter
├── overlay/         Three-layer qcow2 filesystem
├── network/         Per-VM tc-based QoS
├── guest-agent/     Host↔guest relay
├── cli/             terra CLI
└── mcp/             MCP Server (planned)

sdk/python/          Python SDK

thirdparty/          Third-party deps + patch registry
images/              Guest kernel + rootfs build
```

## Roadmap

- **M0** ✅ CH base, guest images, baseline measurements
- **M1** ✅ Engine daemon, CLI, VM lifecycle, qcow2 overlay
- **M2** 🔄 Adapter layer, Sandlock/OpenShell, warm pool, scheduler, Python SDK
- **M3** 🔲 Multi-node placement, PSI/DAMON closed-loop, sched_ext scheduling
- **M4** 🔲 eBPF observability, snapshot fault tolerance, density benchmarks

## License

Apache 2.0. Built on Cloud Hypervisor and Linux kernel features. See `THIRD-PARTY` for acknowledgments.

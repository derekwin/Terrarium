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

## Quick Start（Python SDK / MCP 用户视角）

安装 SDK：

```bash
pip install -e sdk/python
```

**你唯一需要知道的概念是 `layers`**：环境层的名字列表，如 `["python312", "base"]`（前面是工具层，最后是系统层）。其余一切（daemon、二进制、目录）都是自动的。

### 方式 A：单用户，SDK 全自动

零准备——SDK 自动解决引擎、二进制和目录，用完自动清理：

```python
from terra.daemon import Daemon
from terra.client import TerraClient

with Daemon():
    c = TerraClient()

    # 从预热池拿一台 VM（层自动挂载）
    claim = c.pool_claim(["python312", "base"])
    name = claim["name"]

    # 在 VM 里跑命令
    print(c.vm_exec(name, ["python3", "-c", "import numpy; print(numpy.__version__)"]))

    # 归还池
    c.pool_release(name)
```

### 方式 B：服务器已有 daemon（客户端使用）

管理员在服务器上跑着 daemon 时，你只连它用：

```python
from terra.client import TerraClient
from terra.vm import create

c = TerraClient()          # 默认 socket；远程可用 TerraClient("/path/forwarded.sock")

# 创建一台带环境的 VM
vm = create("dev", "target/guest/vmlinux.bin",
            initramfs="target/guest/initramfs-virtiofs.cpio.gz",
            layers=["python312", "base"], cpus=2, memory_mb=512, net=True)

print(vm.info())                                # state / cpus / memory_mb
print(vm.exec(["python3", "--version"]))        # 在 VM 里执行
vm.resize(cpus=4)                               # 在线扩容
vm.destroy()

# 或用预热池（更快的路径）
claim = c.pool_claim(["python312", "base"])
print(c.vm_exec(claim["name"], ["python3", "-c", "print(2**10)"]))
c.pool_release(claim["name"])
```

API 速查（`TerraClient` / `Vm`）：

| 类别 | 方法 |
|---|---|
| VM | `vm_create / vm_list / vm_info / vm_resize / vm_shutdown / vm_kill / vm_destroy` |
| 执行 | `vm_exec(name, args, timeout_secs=60)` |
| 池 | `pool_claim / pool_list / pool_release` |

### MCP（给 AI Agent 用）

MCP Server 以 stdio 运行，直接配进你的 agent（Claude Code / Desktop 等）：

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

Agent 侧可见的用户面工具：`terra_vm_create/list/info/resize/shutdown/kill/destroy`、`terra_exec`、`terra_pool_claim/list/release`、`terra_attach_fs/detach_fs`。典型调用流：

```
terra_pool_claim(layers=["python312","base"])
  → terra_exec(name, args=["python3","-c","print(2**10)"])
  → terra_pool_release(name)
```

> 管理员操作（daemon 启停、镜像构建、网络拆除、建池）不属于用户面——见 `terra` CLI 与 `AGENTS.md`。

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

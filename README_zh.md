# Terrarium Engine

**生产级 Agent 沙箱 — 高单机密度部署安全隔离的执行环境。**

Terrarium Engine 是一个与具体 VM/沙箱技术解耦的调度控制层，把任意 Linux 宿主机变成多租户 Agent 运行时——硬件级 VM 隔离、可插拔沙箱后端、分层 virtiofs 文件系统（EROFS + OverlayFS，设计见 docs/plans）。

## 为什么选择 Terrarium

生产环境运行 AI Agent = 大规模运行不受信代码。容器共享内核，传统 VM 启动慢。Terrarium 取两者之长：

| | 容器 | 纯 VM | Terrarium |
|---|---|---|---|
| 隔离性 | 弱（共享内核） | 强（KVM） | **强（KVM + 沙箱）** |
| 密度 | 高 | 低 | **高** |
| 启动速度 | 快 | 慢（~1s） | **快（CH ~1s，预热池更快）** |
| 文件持久化 | 临时 | 磁盘镜像 | **分层 virtiofs（页缓存共享）** |
| 资源控制 | cgroup | VM 配置 | **tc 网络 QoS** |
| 沙箱后端 | — | — | **可插拔（Sandlock、OpenShell）** |

## 架构

```
┌─ 宿主机 ─────────────────────────────────────────────────────┐
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
│  │  │  guest-proxy ← host→guest 中继        │  │            │
│  │  │  sandlock CLI / openshell CLI         │  │            │
│  │  │  Agent 进程 ◄── 沙箱隔离              │  │            │
│  │  └───────────────────────────────────────┘  │            │
│  │  每 VM: virtiofs rootfs（EROFS 层 + 独立 upperdir）        │            │
│  └─────────────────────────────────────────────┘            │
│  Adapter trait 解耦，多后端                                    │
└──────────────────────────────────────────────────────────────┘
```

**两层 Adapter（trait 定义）：**

| Trait | 职责 | 实现 |
|---|---|---|
| `VmAdapter` | VM 创建、扩缩、快照 | Cloud Hypervisor（Firecracker 已移除——不支持 virtiofs） |
| `SandboxAdapter` | 沙箱创建、执行、销毁 | Sandlock、OpenShell |

## 核心能力

### Adapter trait 架构

- Engine 经 `VmAdapter` / `VmHandle` trait 与 VMM 实现完全解耦
- 可插拔后端：Cloud Hypervisor、Sandlock、OpenShell
- 全 adapter 统一错误类型（`AdapterError`）
- tokio 异步运行时支撑并发 VM 操作

### 分层文件系统（virtiofs）

```
  upperdir   （可写，per-VM 宿主目录）      ← 用户数据
  tool layers（只读 EROFS，按需组合）       ← 工具/运行时
  base layer （只读 EROFS，共享）           ← 系统层
```

只读层在宿主侧经 OverlayFS 星型组合（任意搭配、page cache 共享），经 virtiofs + `cache=always` 暴露给 VM（注意：DAX 已从 Cloud Hypervisor 移除——见 docs/fs-m2-benchmark.md）；写操作 copy-up 进独立 upperdir。计算与数据生命周期分离：VM 命令永不删除数据。设计见 docs/plans。

### 可插拔沙箱后端

| 后端 | 隔离 | 特点 |
|---|---|---|
| **Sandlock** | Landlock + seccomp-bpf + seccomp notif | 免 root、COW FS、HTTP ACL、~5ms 启动（能力门控：宿主 Landlock ABI ≥ v5） |
| **OpenShell** (NVIDIA) | Container + Landlock + OPA proxy | 推理路由、凭据注入、GPU |
| **guest-proxy** | 瘦中继 | host↔guest 命令转发（vsock） |

### 资源控制

- 网络 QoS：per-VM 出/入向限速与优先级（Linux tc）
- 动态扩缩：CPU、内存免重启在线调整（实测 CPU 100%、内存 100% 生效）
- 网络：tap + 宿主 NAT + dnsmasq DHCP，`create --net` 即用，`net-list`/`net-down` 管理
- 执行超时 + 输出上限：每条命令默认 60s（可调至 3600s）+ 10MB 输出上限

## 快速上手（Python SDK / MCP 用户视角）

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

## 仓库结构

```
crates/
├── engine/          引擎 daemon + VM 生命周期 + 池管理
├── adapter/
│   ├── traits/      VmAdapter + SandboxAdapter trait 定义
│   ├── cloud-hypervisor/  CH adapter（异步 client + virtiofs 组合栈）
│   ├── sandlock/    Sandlock adapter（能力门控）
│   └── openshell/   OpenShell adapter
├── protocol/        共享 Command/Response 类型（JSON 协议）
├── guest-proxy/     host↔guest 命令中继（vsock + unix socket）
├── network/         tap/NAT/DHCP + tc QoS
├── cli/             terra CLI（含 image 构建命令）
└── mcp/             MCP Server（stdio JSON-RPC）

sdk/python/          Python SDK（零配置托管：daemon/assets/images/paths）

thirdparty/          第三方依赖 + 补丁登记
images/              guest 内核 + rootfs + initramfs 构建脚本
```

## 路线图

- **M0** ✅ CH 基座、guest 镜像、基线实测
- **M1** ✅ 引擎 daemon、CLI、VM 生命周期
- **M2** ✅ Adapter 层、异步运行时、Python SDK
- **M3** ✅ virtiofs 文件系统（EROFS 层 + OverlayFS）、预热池、网络（tap/NAT/DHCP）、工具层「做中建」
- **M4** 🔲 池自动扩缩、快照容错、密度基准、可观测性

## 许可证

Apache-2.0。基于 Cloud Hypervisor 与 Linux 内核特性构建。致谢见 `THIRD-PARTY`。

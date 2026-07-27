# Terrarium Engine

**生产级 Agent 沙箱 — 高单机密度部署安全隔离的执行环境。**

Terrarium Engine 是一个Agent运行时执行环境的调度控制层，与具体 VM/沙箱技术解耦 —— 硬件级 VM 隔离、可插拔沙箱后端、分层 virtiofs 文件系统。

## 为什么选择 Terrarium

生产环境运行 AI Agent = 大规模运行不受信代码。容器共享内核，传统 VM 启动慢。Terrarium 取两者之长：

| | 容器 | 纯 VM | Terrarium |
|---|---|---|---|
| 隔离性 | 弱（共享内核） | 强（KVM） | **强（KVM + 沙箱）** |
| 密度 | 高 | 低 | **高** |
| 启动速度 | 快 | 慢（~1s） | **快（预热池快速启动）** |
| 文件持久化 | 临时 | 磁盘镜像 | **分层 virtiofs ** |
| 资源控制 | cgroup | VM 配置 | **tc QoS** |
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
| `VmAdapter` | VM 创建、扩缩、快照 | Cloud Hypervisor |
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

只读层在宿主侧经 OverlayFS 星型组合（任意搭配、page cache 共享），经 virtiofs + `cache=always` 暴露给 VM；写操作 copy-up 进独立 upperdir。计算与数据生命周期分离：VM 命令永不删除数据。

### 可插拔沙箱后端

| 后端 | 隔离 | 特点 |
|---|---|---|
| **Sandlock** | Landlock + seccomp-bpf + seccomp notif | 免 root、COW FS、HTTP ACL、~5ms 启动（需要宿主 Landlock ABI ≥ v5） |
| **OpenShell** (NVIDIA) | Container + Landlock + OPA proxy | 推理路由、凭据注入、GPU |
| **guest-proxy** | 中继 | host↔guest 命令转发（vsock） |

### 资源控制

- 网络 QoS：per-VM 出/入向限速与优先级（Linux tc）
- 动态扩缩：CPU、内存免重启在线调整（实测 CPU 100%、内存 100% 生效）
- 网络：tap + 宿主 NAT + dnsmasq DHCP，`create --net` 即用，`net-list`/`net-down` 管理
- 执行超时 + 输出上限：每条命令默认 60s（可调至 3600s）+ 10MB 输出上限

## 快速上手 — 三种使用方式

### 1. `terra` CLI — 管理员工具（docker 风格）

面向宿主管理员：管理 daemon、镜像、网络、预热池，查看一切资源。

```bash
# daemon（本机使用无需 root；网络功能需要）
target/release/engine daemon

# 支持远程的 daemon（TCP + token 门控）
TERRA_TOKEN=secret target/release/engine daemon --tcp 0.0.0.0:19099

terra image kernel --version 6.12     # 构建 guest 内核
terra image layer-build python312 \
    --script images/examples/python312.sh   # 工具层「做中建」：
                                        # builder VM 里配环境，改动即层
                                        # （案例见 images/examples/）
terra image layers                    # 列出可用层
terra pool-create --size 3            # 预热池
terra create dev --kernel ... --initramfs ... --layers python312,base --net
terra list / info dev / resize dev --cpus 4
terra net-list / net-down             # 网络管理
terra destroy dev
```

### 2. Python 直连模式 — 随手开临时 VM，无需任何概念

零准备零概念：不用关心 daemon、session、pool。SDK 首次调用时惰性
启动托管引擎，进程退出自动清理。适合脚本、notebook、本地 agent。

```bash
pip install -e sdk/python
```

```python
import terra

vm = terra.create(layers=["python312", "base"])
print(vm.exec(["python3", "-c", "import numpy; print(numpy.__version__)"]))
vm.destroy()
```

想控制但不写 daemon 代码？用 HostConfig 声明一次——镜像、层、池
大小、VM 默认值、token——你的 Python 脚本就是 daemon 程序：

```python
from terra import HostConfig, create

terra.configure(HostConfig(kernel="~/img/vmlinux.bin",
                           layer_dir="~/layers",
                           pool_size=4,
                           default_net=True))
vm = create(layers=["python312", "base"])
```

### 3. 客户端-服务器模式 — 使用远程 daemon 的 VM 池

管理员在服务器上跑 daemon，你只连它用。

服务器（管理员）——一个 Python 脚本就是 daemon 程序；引擎运行时
自动获取，无需处理二进制：

```python
from terra.daemon import Daemon
from terra.config import HostConfig

cfg = HostConfig(
    kernel="target/guest/vmlinux.bin",
    agent_initramfs="target/guest/initramfs-agent.cpio.gz",
    layer_dir="/var/lib/terra/layers",
    pool_size=4,
    default_net=True,
    token="secret",
)
Daemon(config=cfg, tcp="0.0.0.0:19099").start()   # 常驻服务
```

一次性准备（任何有仓库的机器上跑一次）：
`python3 -c "from terra.assets import publish_engine; publish_engine()"`

客户端（你）——**代码与本地模式完全一致**，只在开头多一行 connect。
创建由服务器的预热池兑现，exec 与 destroy（自动归还池）写法不变：

```python
import terra

terra.connect("tcp://server-ip:19099", token="secret")

vm = terra.create(layers=["python312", "base"])  # 底层走 pool_claim
print(vm.exec(["python3", "--version"]))
vm.destroy()                                    # 底层走 pool_release
```

需要底层控制时仍有完整 client API（`TerraClient`、`pool_claim`、
`vm_create` 等）。CLI 等价：
`TERRA_TOKEN=secret terra --socket tcp://server-ip:19099 list`

> TCP 是明文 + 共享 token 基础访问控制——仅限可信网络。不可信网络
> 请用 SSH 隧道转发 unix socket：
> `ssh -N -L /tmp/terra.sock:/tmp/terra.sock user@server`。

> 管理员操作（daemon 启停、镜像构建、网络拆除、建池）在 `terra` CLI。
> MCP 集成：

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

用户面工具：`terra_vm_create/list/info/resize/shutdown/kill/destroy`、`terra_exec`、`terra_pool_claim/list/release`、`terra_attach_fs/detach_fs`。典型调用流：

```
terra_pool_claim(layers=["python312","base"])
  → terra_exec(name, args=["python3","-c","print(2**10)"])
  → terra_pool_release(name)
```

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

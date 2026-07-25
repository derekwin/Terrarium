# Terrarium Engine

**生产级 Agent 沙箱引擎 — 高单机密度、安全隔离、即插即用。**

Terrarium Engine 是一个与具体 VM/沙箱技术解耦的调度控制层。它把任意 Linux 宿主机变成多租户 Agent 运行时——硬件级 VM 隔离、可插拔沙箱后端、秒级预热池供给。

## 为什么选择 Terrarium

生产环境中运行 AI Agent = 大规模运行不受信代码。容器共享内核，传统 VM 启动慢。Terrarium 取两者之长：

| | 容器 | 纯 VM | Terrarium |
|---|---|---|---|
| 隔离性 | 弱（共享内核） | 强（KVM） | **强（KVM + 沙箱双层）** |
| 密度 | 高 | 低 | **高** |
| 启动速度 | 快 | 慢（~1s） | **快（预热池 ~100ms）** |
| 文件持久化 | 临时 | 磁盘镜像 | **便携 qcow2 overlay** |
| 资源控制 | cgroup | VM 配置 | **cgroup v2 + 网络 QoS** |
| 沙箱后端 | — | — | **可插拔（Sandlock、OpenShell）** |

## 架构

```
┌─ 宿主机 ────────────────────────────────────────────────────┐
│                                                              │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                Terrarium Engine                       │   │
│  │     调度器 · 预热池 · cgroup · 放置决策               │   │
│  │     daemon · CLI · Python SDK · 文件 API              │   │
│  └──────────────────────┬───────────────────────────────┘   │
│                         │                                    │
│          ┌──────────────┴──────────────┐                     │
│          ▼                             ▼                     │
│  ┌───────────────┐            ┌────────────────┐            │
│  │  CH 适配器    │            │  沙箱适配器     │            │
│  │  (VmAdapter)  │            │  Sandlock/OpenShell│         │
│  └───────┬───────┘            └───────┬────────┘            │
│          │                            │                      │
│          ▼                            ▼                      │
│  ┌─────────────────────────────────────────────┐            │
│  │          Cloud Hypervisor VM × N            │            │
│  │  ┌───────────────────────────────────────┐  │            │
│  │  │  guest-agent ← 宿主机↔Guest 命令转发  │  │            │
│  │  │  sandlock CLI / openshell CLI         │  │            │
│  │  │  Agent 进程 ◄── 沙箱隔离执行           │  │            │
│  │  └───────────────────────────────────────┘  │            │
│  │  每 VM: 独立 qcow2 overlay · cgroup · QoS   │            │
│  └─────────────────────────────────────────────┘            │
│                                                              │
│  单机 100+ VM · 预热池 ~100ms 启动 · 资源动态扩缩            │
└──────────────────────────────────────────────────────────────┘
```

**两层 Adapter，trait 解耦：**

| Trait | 职责 | 实现 |
|---|---|---|
| `VmAdapter` | 派生、resize、快照 VM | Cloud Hypervisor（Firecracker、K8s Pod 规划中） |
| `SandboxAdapter` | 创建、执行、销毁沙箱 | Sandlock、OpenShell |

## 核心能力

### 高密度调度

- 单机 100+ VM，每个 TAP 独立网络 QoS
- 准入控制：创建前预检资源可用性
- 空闲回收：自动缩容闲置 VM
- 预热池：CH 快照恢复 → VM 就绪 ~100ms

### 三层文件系统

```
  user-data.qcow2    （读写，per-user，可迁移）     ← 用户数据层
  tools.qcow2         （只读，共享，可组合）          ← 工具层
  rootfs.qcow2        （只读，共享）                  ← 系统层
```
qcow2 backing chain：写操作落到用户层，读操作逐层回退。`scp` 用户层到任意机器，继续工作。

### 可插拔沙箱后端

| 后端 | 隔离方式 | 特色 |
|---|---|---|
| **Sandlock** | Landlock + seccomp-bpf + seccomp 通知 | 无需 root，COW 文件系统，HTTP ACL，~5ms 启动 |
| **OpenShell**（NVIDIA） | 容器 + Landlock + OPA 代理 | 推理路由，凭证注入，GPU |
| **guest-agent** | 命令转发 | Host↔Guest 通信桥 |

### 资源控制

- cgroup v2：per-sandbox 内存限制 + CPU 权重（内核强制执行，接近零开销）
- 网络 QoS：per-VM 出/入带宽限制（Linux tc）
- 动态扩缩：CPU、内存运行时在线调整

## 快速开始

```bash
# 安装 CH（官方 release binary）
wget https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/cloud-hypervisor-static
chmod +x cloud-hypervisor-static

# 构建 Guest 镜像
cd images && bash build.sh

# 启动引擎 daemon
cargo run -p engine --release -- daemon

# 创建 VM
terra vm create agent-1 \
  --kernel target/guest/vmlinux.bin \
  --rootfs-disk /data/base.qcow2 \
  --toolfs-disk /data/tool-python.qcow2

# 在沙箱中运行 Agent
terra sandbox exec python3 -c "print(2 ** 10)"
```

### Python SDK

```python
import terra

# 创建 VM
vm = terra.vm.create("agent-1", kernel="target/guest/vmlinux.bin")

# 在沙箱中执行
sb = terra.sandbox.create("agent-1", tools=["python"])
result = sb.exec("python3", "-c", "print(2 ** 10)")
print(result.stdout)  # 1024

# 读取 Agent 输出
content = sb.read_file("/home/agent/output.txt")
```

## 仓库结构

```
crates/
├── engine/          调度器、预热池、cgroup、daemon
├── adapter/
│   ├── traits/      VmAdapter + SandboxAdapter
│   ├── ch/          Cloud Hypervisor（自包含）
│   ├── sandlock/    Sandlock 适配器
│   └── openshell/   OpenShell 适配器
├── overlay/         三层 qcow2 文件系统
├── network/         per-VM tc 流量控制
├── guest-agent/     Host↔Guest 命令转发
├── cli/             terra CLI
└── mcp/             MCP Server（规划中）

sdk/python/          Python SDK

thirdparty/          第三方依赖 + 补丁登记
images/              Guest 内核 + rootfs 构建
```

## 路线图

- **M0** ✅ CH 基座、Guest 镜像、基线实测
- **M1** ✅ 引擎 daemon、CLI、VM 生命周期、qcow2 overlay
- **M2** 🔄 Adapter 层、Sandlock/OpenShell、预热池、调度器、Python SDK
- **M3** 🔲 多机放置、PSI/DAMON 闭环、sched_ext 调度
- **M4** 🔲 eBPF 观测、快照容错、密度压测

## 许可证

Apache 2.0。构建于 Cloud Hypervisor 及 Linux 内核特性之上。详见 `THIRD-PARTY`。

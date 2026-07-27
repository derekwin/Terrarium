# Terrarium Engine

**生产级 Agent 沙箱——高单机密度部署安全隔离的执行环境。**

Terrarium 是 Agent 执行环境的调度控制层。它将硬件级 VM 隔离（Cloud Hypervisor）与可组合的分层文件系统（EROFS + OverlayFS + virtiofs）和预热池结合：不受信的 Agent 代码运行在真实 VM 中，秒级就绪，并在宿主机上共享只读环境层。

## 为什么选择 Terrarium

| | 容器 | 微 VM | Terrarium |
|---|---|---|---|
| 隔离性 | 共享内核 | KVM | **KVM + 沙箱** |
| 密度 | 高 | 低 | **高（页缓存共享的层）** |
| 供给 | 快 | ~1s | **预热池（预启动 VM）** |
| 环境 | OCI 镜像 | 磁盘镜像 | **可组合的命名层** |
| 后端 | — | — | **可插拔（CH、Sandlock、OpenShell）** |

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
│  │  │  Agent 进程 ◄── 沙箱隔离              │  │            │
│  │  └───────────────────────────────────────┘  │            │
│  │  每 VM: virtiofs rootfs（层 + /workdir）     │            │
│  └─────────────────────────────────────────────┘            │
└──────────────────────────────────────────────────────────────┘
```

引擎通过两组 trait 与后端解耦：`VmAdapter`（Cloud Hypervisor）与
`SandboxAdapter`（Sandlock、OpenShell）。

## 快速上手

安装 CLI 与 SDK：

```bash
pip install -e sdk/python
```

**CLI**——资源分组，动词统一为 `ls / create / remove`：

```bash
sudo env "PATH=$PATH" terra daemon start                       # 引擎 daemon（root 才有 NAT 网络）
terra kernel create -n k612 --version 6.12      # 构建 guest 内核
terra layer create -n python312 --script images/examples/python312.sh
terra pool create --size 3                      # 预热池
terra vm create dev --kernel k612 --rootfs alpine --layers python312,base --net
terra vm exec dev -- python3 --version
terra vm remove dev
```

**Python**——直连模式，随手开临时 VM：

```python
import terra

vm = terra.create(layers=["python312", "base"])
print(vm.exec(["python3", "-c", "import numpy; print(numpy.__version__)"]))
vm.destroy()
```

**客户端-服务器**——代码不变，一行 connect，由服务器预热池兑现：

```python
import terra

terra.connect("tcp://server:19099", token="secret")

vm = terra.create(layers=["python312", "base"])
print(vm.exec(["python3", "--version"]))
vm.destroy()
```

**MCP**——将 agent 指向 stdio server：

```json
{"mcpServers": {"terrarium": {"command": "terra-mcp", "env": {"TERRA_SOCKET": "/tmp/terra.sock"}}}}
```

## 特性

- **分层文件系统**——只读 EROFS 层在宿主侧星型组合（任意搭配、页缓存共享），经 virtiofs 暴露。工具层通过在真实 VM 中配置环境、打包增量来构建——环境自证可用。
- **预热池**——预启动的空转 VM；认领时热插所需层并返回就绪 VM，任务结束归还复用。
- **guest 内执行**——经 guest agent 在 VM 内执行命令，支持单命令超时。
- **网络**——`--net` 一键 NAT 联网（DHCP 即用），生命周期经 `terra net` 管理。
- **动态扩缩**——CPU、内存免重启在线调整。
- **零配置 Python SDK**——托管目录、二进制与镜像自动解析、可编程宿主配置（`HostConfig`）。

## 文档

- [docs/protocol.md](docs/protocol.md)——引擎线协议（命令、传输、语义）
- [docs/sdk.md](docs/sdk.md)——Python SDK 与 CLI 参考
- [docs/mcp.md](docs/mcp.md)——MCP 工具面
- [docs/plans/](docs/plans/)——设计 ADR

## 路线图

- ✅ CH 基座、引擎 daemon、Adapter 层、Python SDK
- ✅ virtiofs 分层文件系统、预热池、NAT 网络、工具层「做中建」
- 🔲 池自动扩缩、快照容错、密度基准、可观测性

## 许可证

Apache-2.0。基于 Cloud Hypervisor 与 Linux 内核特性构建。致谢见 `THIRD-PARTY`。

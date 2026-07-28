# Terrarium Engine

**生产级 Agent 沙箱——高单机密度部署安全隔离的执行环境。**

Terrarium 是 Agent 执行环境的调度控制层。它将硬件级 VM 隔离（Cloud Hypervisor）与可组合的分层文件系统（EROFS + OverlayFS + virtiofs）和预热池结合：不受信的 Agent 代码运行在真实 VM 中，秒级就绪，并在宿主机上共享只读环境层。

**租户优先模型：** VM 是租户隔离边界，Sandbox 是 VM 内的会话，而非 VM 本身。同一租户下的多个 Sandbox 共享一个 VM，各自拥有独立的工作目录。这样既保证了租户间的强安全隔离，又实现了租户内的高密度部署。

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
│  │      Cloud Hypervisor VM（每租户一个）       │            │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐    │            │
│  │  │ Sandbox  │ │ Sandbox  │ │ Sandbox  │    │            │
│  │  │ /workdir │ │ /workdir │ │ /workdir │    │            │
│  │  │ 会话     │ │ 会话     │ │ 会话     │    │            │
│  │  └──────────┘ └──────────┘ └──────────┘    │            │
│  │  guest-proxy（vsock 中继，每 VM）           │            │
│  │  virtiofs rootfs（共享层 + 各 sb 工作目录） │            │
│  └─────────────────────────────────────────────┘            │
└──────────────────────────────────────────────────────────────┘
```

引擎通过两组 trait 与后端解耦：`VmAdapter`（Cloud Hypervisor）与
`SandboxAdapter`（Sandlock、OpenShell）。

## 仓库结构

```
terrarium/
├── README.md / README_zh.md
├── LICENSE / NOTICE / THIRD-PARTY
├── crates/
│   ├── engine/               # 引擎 daemon：PyO3 库，7 个命令子模块
│   ├── adapter/
│   │   ├── traits/           # VmAdapter / SandboxAdapter trait、VmSpec、FsSpec、错误类型
│   │   ├── cloud-hypervisor/ # CH adapter：5 模块（FS/VM 解耦）、virtiofs、热插、网络、landlock
│   │   ├── sandlock/         # Sandlock adapter（Landlock ABI 能力门控）
│   │   └── openshell/        # OpenShell adapter
│   ├── fs/                   # 独立文件系统 crate：EROFS、cpio、层构建/列举/删除（PyO3 绑定）
│   ├── protocol/             # 共享 Command / Response 类型（单一事实源）
│   ├── guest-proxy/          # guest 内代理：vsock 中继、exec、mount、umount
│   ├── network/              # Tap / NAT / dnsmasq DHCP、tc QoS
│   └── mcp/                  # MCP server（stdio JSON-RPC，13 个用户面工具）
├── sdk/python/               # Python SDK（terra 包：Sandbox、Pool、Template、client、daemon、assets、images）
├── images/                   # Guest 内核 / rootfs / initramfs 构建脚本与示例
└── docs/                     # 协议、SDK、MCP 文档与设计 ADR
```

## 快速上手

从仓库根目录安装：

```bash
pip install -e .
```

**Sandbox API**——推荐的高层入口（自动起 daemon，context manager 自动回收）：

```python
from terra.sandbox import Sandbox

# Sandbox = 租户 VM 内的会话（租户优先模型）
with Sandbox(tenant="my-org", template="py312", network=True) as sb:
    result = sb.exec(["python3", "-c", "print(2+2)"])
    print(result.stdout)              # "4\n"
    sb.files.write("/workdir/hello.txt", "Hello, Terrarium!")
    print(sb.files.read("/workdir/hello.txt"))
    print(sb.id)                      # "tenant-my-org/sb-a3f2"
    print(sb.vm)                       # "tenant-my-org"

# 多个 Sandbox 共享一个租户 VM
sb1 = Sandbox(tenant="research-team", template="py312")
sb2 = Sandbox(tenant="research-team")   # 复用同一 VM，新工作目录
sb3 = Sandbox(tenant="research-team")

sb1.kill()  # 仅移除会话工作目录——VM 仍保留供其他 Sandbox 使用
Sandbox.destroy_tenant("research-team")  # 销毁 VM 及所有会话
```

**预热池**——预启动共享租户 VM，claim 时热插层即用：

```python
from terra.pool import Pool

pool = Pool(template="py312", size=3)  # 3 个预热 VM
sb1 = pool.acquire()                   # 共享池 VM 中的 Sandbox
sb2 = pool.acquire()                   # 同一 VM，不同工作目录
print(sb1.exec(["python3", "--version"]).stdout)
pool.release(sb1)                      # 归还池
pool.release(sb2)
```

**CLI**——资源分组，动词统一：

```bash
sudo env "PATH=$PATH" terra daemon start                     # 引擎 daemon（root 才有 NAT 网络）
terra image build kernel -n k612 --version 6.12               # 构建 guest 内核
terra image build rootfs -n alpine                             # 构建发行版系统
terra layer create -n python312 --rootfs alpine --script images/examples/python312.sh --kernel k612
terra template create -n py312 --kernel k612 --rootfs alpine --layers python312,base
terra sandbox create --tenant research-team --template py312 --net   # 租户优先沙箱
terra sandbox kill tenant-research-team/sb-a3f2                        # 终止单个会话
terra sandbox destroy-tenant research-team                             # 销毁 VM + 全部会话
terra pool create -n mypool --size 3                                   # 预热池
terra pool claim --template py312                                      # 认领就绪 Sandbox
terra daemon config                                            # 引擎/池/网络/层一览
```

**MCP**——将 agent 指向 stdio server：

```json
{"mcpServers": {"terrarium": {"command": "terra-mcp", "env": {"TERRA_SOCKET": "/tmp/terra.sock"}}}}
```

## 特性

- **高层 Sandbox API**——`terra.Sandbox` / `terra.AsyncSandbox`，采用租户优先模型：VM 是租户隔离边界，Sandbox 是 VM 内的会话。同一租户的多个 Sandbox 共享一个 VM，各自拥有独立工作目录。自动起 daemon，context manager 自动清理，支持文件操作（读/写/上传/下载/列举）、资源指标、在线扩缩。
- **分层文件系统**——只读 EROFS 层在宿主侧星型组合（任意搭配、页缓存共享），经 virtiofs 暴露。发行版系统层来自配置驱动 pipeline（内置 alpine 与 ubuntu，新增仅需三行配置）。工具层通过在真实 VM 中配置环境、打包增量来构建——环境自证可用。
- **预热池**——预启动的空转 VM 作为共享租户容器；acquire 返回池 VM 内的 Sandbox 会话。同一池的多次 acquire 共享同一 VM，各自拥有独立工作目录。任务结束归还复用。支持动态 `grow()` / `scale` 实时调整池大小。
- **命名模板**——`terra.template.Template` 持久化内核 + 基础系统 + 工具层组合，CLI 与 SDK 统一通过名称引用。
- **guest 内执行**——blocking 与 background 两种模式，经 guest agent 在 VM 内执行命令。支持会话追踪（`session_status`、`session_kill`、`session_list`）、单命令超时、结构化 `ExecResult`。
- **网络**——`--net` 一键 NAT 联网（DHCP 即用），生命周期经 `terra net` 管理。
- **动态扩缩**——CPU、内存免重启在线调整。
- **零配置 Python SDK**——托管目录、二进制与镜像自动解析、daemon 自动启动、可编程宿主配置（`HostConfig`）。

## 文档

- [docs/protocol.md](docs/protocol.md)——引擎线协议（命令、传输、语义）
- [docs/sdk.md](docs/sdk.md)——Python SDK 与 CLI 参考
- [docs/mcp.md](docs/mcp.md)——MCP 工具面
- [docs/plans/](docs/plans/)——设计 ADR

## 路线图

- ✅ CH 基座、引擎 daemon、Adapter 层、Python SDK
- ✅ virtiofs 分层文件系统、预热池、NAT 网络、工具层「做中建」
- ✅ 高层 Sandbox / Pool / Template API，异常体系，async 支持
- 🔲 池自动扩缩、快照容错、密度基准、可观测性

## 许可证

Apache-2.0。基于 Cloud Hypervisor 与 Linux 内核特性构建。致谢见 `THIRD-PARTY`。

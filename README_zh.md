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
| 后端 | — | — | **CH 微 VM + guest 内 Sandlock（`SandboxAdapter` trait 预留给未来后端）** |

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
│  │  (VmAdapter)  │            │    Sandlock     │          │
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
`SandboxAdapter`（当前为 Sandlock；trait 保留为未来沙箱后端的扩展点）。

## 仓库结构

```
terrarium/
├── README.md / README_zh.md
├── LICENSE / NOTICE / THIRD-PARTY
├── crates/
│   ├── engine/               # 引擎 daemon：PyO3 库，7 个命令子模块
│   ├── adapter/
│   │   ├── traits/           # VmAdapter / SandboxAdapter trait、VmSpec、FsSpec、错误类型
│   │   ├── cloud-hypervisor/ # CH adapter（FS/VM 解耦）、virtiofs、热插、网络、landlock
│   │   └── sandlock/         # Sandlock adapter（Landlock/seccomp 权限隔离；SandboxAdapter 参考实现）
│   ├── fs/                   # 独立文件系统 crate：EROFS、cpio、层构建/列举/删除（PyO3 绑定）
│   ├── protocol/             # 共享 Command / Response 类型（单一事实源）
│   ├── guest-proxy/          # guest 内代理：vsock 中继、exec、mount、umount
│   ├── network/              # Tap / NAT / dnsmasq DHCP、tc QoS
│   └── mcp/                  # MCP server（stdio JSON-RPC，15 个用户面工具）
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
with Sandbox(tenant="my-org", template="alpine", network=True) as sb:
    result = sb.exec("echo hello")
    print(result.stdout)              # "hello\n"
    sb.files.write("/workdir/hello.txt", "Hello, Terrarium!")
    print(sb.files.read("/workdir/hello.txt"))
    print(sb.id)                      # "sb-a3f2b1c4"（引擎分配的 id）
    print(sb.vm)                       # "tenant-my-org"

# 多个 Sandbox 共享一个租户 VM
sb1 = Sandbox(tenant="research-team", template="alpine")
sb2 = Sandbox(tenant="research-team")   # 复用同一 VM，新工作目录
sb3 = Sandbox(tenant="research-team")

sb1.kill()  # 终止本会话（running 会话 + 工作目录）——VM 仍保留供其他 Sandbox 使用
Sandbox.destroy_tenant("research-team")  # 销毁 VM 及所有会话
```

**预热池**——预启动共享租户 VM，claim 时热插层即用：

```python
from terra.pool import Pool

pool = Pool(template="alpine", size=3)  # 3 个预热 VM
sb1 = pool.acquire()                   # 共享池 VM 中的 Sandbox
sb2 = pool.acquire()                   # 同一 VM，不同工作目录
print(sb1.exec(["uname", "-r"]).stdout)
pool.release(sb1)                      # 归还池
pool.release(sb2)
```

**CLI**——三步跑起一个沙箱：

```bash
terra setup alpine                             # 一次性：内核 + rootfs + initramfs + base 层（含 sandlock）+ 模板
terra daemon start                             # 引擎 daemon（自动 sudo 提权；--no-root 免提权）
terra sandbox create --template alpine --net   # 高层沙箱（VM 即租户边界）
terra sandbox exec sb-xxxxxxxx -- echo hi      # 默认经 sandlock 沙箱化执行
terra sandbox kill sb-xxxxxxxx                 # 终止会话（VM 保留）
terra pool create -n mypool --size 3           # 预热池
terra pool claim --template alpine             # 认领就绪 Sandbox
terra daemon status                            # 引擎状态一览
```

`terra setup ubuntu` 对 ubuntu 做同样的事。工具层（python3 之类）基于发行版模板构建：

```bash
terra tool create -n python312 --template alpine --script images/examples/python312.sh
terra tool ls
terra tool remove -n python312
```

工具层同时是 RL 环境基线的机制：把环境的就绪态烘焙进层
（`images/examples/rl-env.sh`），`Batch.reset_in_place()` 只清理 episode
写入的 upper——就绪态在每次重置后依然存活。最小训练循环见
`sdk/python/examples/rl_episode_loop.py`
（注入输入 → 跑层内任务 → 收集结果 → reset_in_place），回归验证见
`sdk/python/tests/manual_envlayer.sh`。

**Agent 执行**——同一套层工具链服务 agent 场景：repo + 工具链 + 测试
烘焙进层，从同一快照恢复 N 个 agent 环境，每个 agent 编辑自己的工作区，
就地重置把每个环境恢复到层基线。已在一个真实 SWE-bench 实例
（`pallets__flask-4160`）上端到端验证：4 个并行环境基线复现 bug，修复后
套件全绿（33 passed），重置后恢复有 bug 的基线
（`sdk/python/tests/manual_swebench.sh`）。批量扩展到 5 个 flask 实例
（覆盖 2.0/2.1/2.3），全部通过
（`sdk/python/tests/manual_swebench_batch.py`，原始结果在
`docs/benchmark-results-2026-08-05-swebench-batch.json`）。

**Agent CI**——agent 产出代码的隔离验证，在本仓库 dogfooding：
`sdk/python/examples/ci_verify.py` 从原始快照恢复、把 repo 复制进
每 VM 工作区、应用 agent 的 patch 并跑测试套件。`ci-terra` 层烘焙
repo + 测试工具链（构建时验证基线套件）。演示：好 patch 通过
（42 passed），坏 patch 被拒（1 failed），每次验证约 1.5s。

**MCP**——将 agent 指向 stdio server：

```json
{"mcpServers": {"terrarium": {"command": "terra-mcp", "env": {"TERRA_SOCKET": "/tmp/terra.sock"}}}}
```

## 特性

- **高层 Sandbox API**——`terra.Sandbox` / `terra.AsyncSandbox`，采用租户优先模型：VM 是租户隔离边界，Sandbox 是 VM 内的会话。Sandbox 是引擎级实体——引擎维护注册表（tenant → VM、sandbox → workdir），所有客户端共享同一视图。同一租户的多个 Sandbox 共享一个 VM，各自拥有独立工作目录。自动起 daemon，context manager 自动清理，支持文件操作（读/写/上传/下载/列举）、资源指标、在线扩缩。
- **双层隔离**——租户之间是 KVM 微 VM；租户 VM 内部，每次 `Sandbox.exec` 都默认经 sandlock（Landlock/seccomp，由 `terra setup` 烘焙进系统层 `/usr/bin/sandlock`）约束运行。默认策略：系统目录只读，仅会话工作目录与 `/tmp` 可写，同 VM 其他会话的工作目录不可达；网络暂不限制。策略可由用户控制——额外文件系统授予、出站白名单（`net_allow`）、内存/进程数限制，在创建 sandbox 时设定或逐次 exec 覆盖（`Sandbox(policy={...})` / CLI `--read-path/--write-path/--net-allow/--memory-mb/--procs`）。可用 `sandboxed=False` / `--no-sandbox` 逐次关闭。
- **分层文件系统**——只读 EROFS 层在宿主侧星型组合（任意搭配、页缓存共享），经 virtiofs 暴露。发行版系统层来自配置驱动 pipeline（内置 alpine 与 ubuntu，新增仅需三行配置）。工具层通过在真实 VM 中配置环境、打包增量来构建——环境自证可用。
- **预热池**——预启动的空转 VM 作为共享租户容器；acquire 返回池 VM 内的 Sandbox 会话。同一池的多次 acquire 共享同一 VM，各自拥有独立工作目录。任务结束归还复用。支持动态 `grow()` / `scale` 实时调整池大小。
- **命名模板**——`terra.template.Template` 持久化内核 + 基础系统 + 工具层组合，由 `terra setup` 或 SDK 写入，统一通过名称引用。
- **guest 内执行**——blocking 与 background 两种模式，经 guest agent 在 VM 内执行命令。支持单命令超时、结构化 `ExecResult`。后台执行会话在协议层可追踪（`session_status`、`session_kill`、`session_list`），但目前没有随附客户端（CLI、SDK、MCP）暴露它们。
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

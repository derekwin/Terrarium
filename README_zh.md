# Terrarium

Terrarium 是面向 AI Agent 工作负载的轻量沙箱平台：**以 microVM 为隔离边界，以进程沙箱为执行单元**，提供安全、弹性、可观测、可容错的 Agent 执行环境。

容器的隔离边界与 Agent 进程同在用户态，对不受信代码的约束有限；传统虚拟机开销大、资源配置静态。Terrarium 结合两者之长：硬件级隔离 + VM 内按 Agent 粒度的沙箱化，配合动态资源模型，按需缩放 CPU、内存和磁盘。

**技术路线**：VMM 基座采用 [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) fork（薄 fork——"能配置就不补丁"），自研控制面与沙箱层构成项目核心 IP。

## 核心功能目标

- **动态资源**：CPU、内存、磁盘在线伸缩，走 Cloud Hypervisor 的 resize API。采用「启动预创建 + 运行调整」模型——vCPU 启动时声明上限、运行中逻辑上下线；virtio-mem 启动时挂载、运行中 config change 调整。不需要 Guest 内核补丁。
- **双层隔离**：
  - **VM 层**：KVM 硬件虚拟化，作为安全边界
  - **沙箱层**：namespace + OverlayFS + cgroup v2 + Landlock + seccomp-bpf，每个 Agent 一个执行单元
- **可观测**：Guest 内 eBPF（CO-RE）按沙箱粒度采集 syscall、文件、网络、资源计量，经 vsock 上报宿主机
- **快照容错**：三级快照——文件系统 CoW 快照、进程级 CRIU（Agent step 边界）、整 VM 快照/恢复（走 Cloud Hypervisor）
- **相位感知调度**：宿主机 sched_ext 调度器在 LLM 推理等待期回收 vCPU 时间片
- **预热池**：预启动 VM 就绪，创建沙箱即认领

## 架构

```
┌─ 宿主机 ────────────────────────────────────────────────────────┐
│  terra-controller daemon（控制面唯一入口）                        │
│  输入：PSI / DAMON / eBPF 计量  →  输出：调 CH resize API        │
│  sched_ext 调度器（LLM 等待期回收 CPU）                           │
│                                                                  │
│  cloud-hypervisor：每 VM 一个进程（controller 通过 unix domain    │
│  socket API 派生与管理）                                         │
│  ┌─ VM（KVM 隔离）────────────────────────────────────────────┐ │
│  │  sandboxd：沙箱生命周期管理                                  │ │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐                    │ │
│  │  │  Agent   │ │  Agent   │ │  Agent   │ ...                │ │
│  │  │  沙箱    │ │  沙箱    │ │  沙箱    │                    │ │
│  │  └──────────┘ └──────────┘ └──────────┘                    │ │
│  │  observe（eBPF 观测）    │  checkpoint daemon               │ │
│  └────────────────────┬──────────────────────────────────────┘ │
│                vsock 控制/观测通道                               │
└─────────────────────────────────────────────────────────────────┘
```

| 层 | 隔离 | 资源 | 监控 | 容错 | 形态 |
|---|---|---|---|---|---|
| **VM** | KVM 硬件虚拟化 | virtio-mem / vCPU / balloon / blk 动态调整 | PSI、DAMON | CH VM 快照/恢复 | 每 VM 一个 CH 进程，controller 管理 |
| **沙箱** | namespace + Landlock + seccomp | cgroup v2 配额与限速 | eBPF 按沙箱粒度 | FS CoW + CRIU（step 边界） | Guest 内常驻 daemon |

## 使用接口（M2 起）

Terrarium 对外的第一公民是沙箱而非 VM：

```python
import terra

with terra.sandbox.create(name='dev', image='python:3.12') as sb:
    proc = sb.exec('python', '-c', 'print(2 ** 10)')
    proc.wait()
    print(proc.stdout.read())          # 1024

    snap = sb.snapshot()

sb2 = terra.sandbox.create(name='dev2', snapshot=snap)
```

- **Python SDK**：`create / exec / terminate / snapshot / pause / resume / resize / ls`，均有 `.aio` 异步版本
- **CLI**：`terra sandbox create / exec / ls / terminate / snapshot / pool`
- **MCP Server**：沙箱能力以 MCP tools 暴露
- **预热池**：`create_pool(image=..., replicas=...)` 创建即认领
- **在线调整**：`pause() / resume()` 整 VM 挂起与恢复；`resize(cpus=..., memory_gb=...)` 运行中伸缩

## 仓库结构

```
terrarium/
├── AGENTS.md / README.md / README_zh.md
├── LICENSE (Apache-2.0) / NOTICE / THIRD-PARTY
├── hypervisor/             # Cloud Hypervisor fork（git submodule 或 vendored 分支）
│   └── PATCHES.md          # 本地补丁登记
├── crates/
│   ├── ch-client/          # CH API socket 客户端（create/start/resize/add-disk/snapshot）
│   ├── controller/         # terra-controller daemon（控制面）
│   ├── sandboxd/           # Guest 内沙箱运行时（M2）
│   ├── observe/            # Guest 内 eBPF 观测 daemon（M2）
│   ├── checkpoint/         # 快照协调（M3）
│   ├── cli/                # terra CLI（M2）
│   └── mcp/                # MCP Server（M2）
├── sdk/python/             # Python SDK（M2）
├── images/                 # Guest 内核配置与 rootfs 构建脚本
├── docs/decisions/         # 架构决策记录（ADR）
└── .github/workflows/      # CI
```

## 路线图

- **M0 — CH 基座与动态资源实测**：fork 引入、Guest 镜像构建、启动基线、CPU/内存/磁盘 resize 三件套实测、ch-client 骨架
- **M1 — Controller 骨架 + 手动资源闭环**：ch-client 完整封装、VM 生命周期管理、手动触发 resize 验证闭环
- **M2 — 沙箱层与开发接口**：sandboxd 全隔离栈、eBPF 观测通道、Python SDK / CLI / MCP Server
- **M3 — 快照容错**：FS CoW 快照 → CH VM 快照/恢复 → 进程级 CRIU
- **M4 — 自动化与密度**：PSI/DAMON 接入闭环决策、sched_ext 调度、预热池、单机密度压测

## 致谢

Terrarium 构建于 [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor)（Apache License 2.0）之上。我们维护一个薄 fork，保持最少、充分文档化的本地补丁。详见 `hypervisor/PATCHES.md` 与 `THIRD-PARTY`。

本项目以 Apache License 2.0 发布。

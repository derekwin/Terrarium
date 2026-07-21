# Terrarium

Terrarium 是一个面向 AI Agent 工作负载的轻量 VMM 与沙箱运行时，目标是提供安全、弹性、可观测、可容错的 Agent 执行环境。

容器的隔离边界与 Agent 进程同在用户态，对不受信代码的约束有限；传统虚拟机开销大、资源配置静态。Terrarium 以 microVM 为隔离边界，以进程沙箱为执行单元。

## 核心功能目标

- **轻量快速**：microVM 冷启动 < 200ms，单实例内存开销 < 100MB，virtio-mmio 极简设备模型，无 PCI/ACPI 依赖
- **动态资源**：CPU / 内存 / 磁盘在线伸缩。采用「启动预创建 + 运行调整」模型——vCPU 按上限预建、Guest 内逻辑上下线；virtio-mem 预挂载、经 config change 调整；不需要 Guest 内核补丁
- **双层隔离**：
  - VM 层：KVM 硬件隔离，作为安全边界
  - 沙箱层：namespace + pivot_root + OverlayFS + cgroup v2 + Landlock + seccomp-bpf，每个 Agent 一个执行单元
- **可观测**：Guest 内 eBPF（CO-RE）按沙箱粒度采集 syscall / 文件 / 网络 / 资源计量，经 vsock 上报宿主机
- **快照容错**：三级快照——文件系统 CoW 快照（毫秒级）、进程级 CRIU（Agent step 边界）、整 VM 快照 + userfaultfd 懒恢复
- **安全管控**：BPF LSM 动态策略，文件路径与网络出口白名单，按会话资源计量

## 架构

```
┌─ Host ─────────────────────────────────────────────────┐
│  资源控制器（Terrarium 以库形态嵌入，无独立 daemon）       │
│  输入：PSI / DAMON 工作集 / eBPF 计量  →  输出：动态调整   │
│  sched_ext 调度器（LLM 等待期回收 CPU）                   │
│                                                        │
│  terra-vmm：每 VM 一个独立进程（controller 派生，API socket 管理）│
│  ┌─ Terrarium VM（microVM，KVM 隔离）─────────────────┐ │
│  │  sandboxd：沙箱生命周期管理                          │ │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐            │ │
│  │  │ Agent 沙箱 │ │ Agent 沙箱 │ │ Agent 沙箱 │ ...      │ │
│  │  └──────────┘ └──────────┘ └──────────┘            │ │
│  │  eBPF 观测 daemon  │  checkpoint daemon             │ │
│  └──────────────────────┬─────────────────────────────┘ │
│                    vsock 控制/观测通道                     │
└────────────────────────────────────────────────────────┘
```

| | VM 层（Terrarium VMM） | 沙箱层（sandboxd） |
|---|---|---|
| 隔离 | KVM 硬件虚拟化 | namespace + Landlock + seccomp |
| 资源 | virtio-mem / balloon / vCPU / blk 动态调整 | cgroup v2 配额与限速 |
| 监控 | VM 级资源画像（PSI / DAMON） | eBPF 按沙箱粒度行为采集 |
| 容错 | 整 VM 快照 + uffd 懒恢复 | FS CoW 快照 + CRIU（step 边界） |
| 形态 | 库（嵌入控制器进程） | Guest 内常驻 daemon |

## 使用接口

Terrarium 对外的第一公民是沙箱而非 VM，开发者只需要一个 `Sandbox` 对象：

```python
import terra

with terra.sandbox.create(name='dev', image='python:3.12') as sb:
    proc = sb.exec('python', '-c', 'print(2 ** 10)')
    proc.wait()
    print(proc.stdout.read())          # 1024

    snap = sb.snapshot()               # 文件系统 + 进程内存全量状态

sb2 = terra.sandbox.create(name='dev2', snapshot=snap)  # 从快照恢复
```

**沙箱如何定位到 VM**：默认模式下，VM 的创建、伸缩与沙箱放置由控制器自动完成（按租户亲和、资源水位做 bin-packing）；`create()` 返回的 `Sandbox` 句柄内部封装了 `(vm, sandbox)` 路由信息，后续 `exec` / `snapshot` 自动定位，开发者无需感知 VM。需要显式控制时（整 VM 挂起、租户独占），VM 也是一等对象：

```python
vm = terra.vm.create(cpus=8, memory_gb=16)   # 独占 VM
sb = vm.sandbox.create(image='python:3.12')
vm.pause()                                   # VM 内所有沙箱一起挂起
```

也可以通过 `terra.sandbox.create(placement=...)` 传入亲和 / 独占提示。

- **Python SDK**：`create / exec / terminate / snapshot / pause / resume / resize / ls`，均有 `.aio` 异步版本；`num_sandboxes=N` 批量创建，适配 RL rollout 与并行 eval
- **CLI**：`terra sandbox create / exec / ls / terminate / snapshot / pool`
- **MCP Server**：沙箱能力以 MCP tools 暴露（create / run / snapshot / terminate），Agent 客户端可直接接入
- **预热池**：`create_pool(image=..., replicas=...)` 保持预启动 microVM，创建即认领；池可基于快照镜像
- **凭据与网络**：`secrets=` / `env=` 创建时注入；沙箱间默认网络隔离，`ports=` 显式暴露后可互访
- **在线调整**：`pause() / resume()` 整 VM 挂起与恢复；`resize(cpus=..., memory_gb=...)` 运行中伸缩

## 模块结构

```
terrarium/
├── crates/
│   ├── vmm-core/       # VM 生命周期、地址空间、vCPU 管理
│   ├── vmm-devices/    # virtio-mmio 设备：blk / virtio-mem / balloon / vsock
│   ├── vmm-snapshot/   # VM 状态序列化 + userfaultfd 懒恢复
│   ├── vmm/            # terra-vmm 可执行文件（组合各 crate 的薄壳，每 VM 一进程）
│   ├── vmm-api/        # controller 与 terra-vmm 之间的 API socket 协议
│   ├── sandboxd/       # Guest 内沙箱运行时：隔离栈、生命周期、快照协调
│   ├── observe/        # Guest 内 eBPF 观测 daemon，vsock 上报
│   ├── checkpoint/     # CRIU 封装与 step 边界静默点协议
│   ├── controller/     # 宿主资源控制器：调度、放置、预热池、资源闭环
│   ├── cli/            # 命令行工具
│   └── mcp/            # MCP Server
├── sdk/python/         # Python SDK（同步 + asyncio）
└── xtask/              # 构建工具：Guest 内核 / rootfs 打包
```

## Roadmap

- **M0 骨架**：基于 rust-vmm 的最小 VMM，直接启动裁剪内核到 shell
- **M1 动态资源**：设备层就绪，「预创建 + 调整」三件套（内存 / CPU / 磁盘）实测通过
- **M2 沙箱层与开发接口**：sandboxd 全隔离栈 + eBPF 观测通道；Python SDK / CLI / MCP Server 可用
- **M3 快照容错**：FS CoW 快照 → 整 VM 快照与懒恢复（SDK 暴露 `snapshot / pause / resume`）→ 进程级 CRIU
- **M4 密度与调度**：sched_ext 调度优化，单机密度压测，资源闭环自动化，预热池上线

## 致谢与许可

Terrarium 构建于 [rust-vmm](https://github.com/rust-vmm) 生态之上，部分设备实现衍生自
[Dragonball](https://github.com/kata-containers/kata-containers)（Apache License 2.0），
详见 `NOTICE` 与 `THIRD-PARTY`。

本项目以 Apache License 2.0 发布。

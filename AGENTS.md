# Terrarium

> 本文件是项目的唯一权威开发指令。仓库中如存在任何旧代码/旧文档与本文件冲突，以本文件为准；旧实现（自研 VMM 路线）已全部废弃，仓库应视为全新起点。
> 动手前先复述你对项目定位、当前里程碑范围和架构红线的理解。

## 1. 项目定位

Terrarium 是面向 AI Agent 工作负载的轻量沙箱平台：**以 microVM 为隔离边界，以进程沙箱为执行单元**，提供安全、弹性、可观测、可容错的 Agent 执行环境。

技术路线（已定，不得动摇）：

- **VMM 基座 = Cloud Hypervisor fork**。CH 已提供全部资源调整执行机制（`vm.resize` 增删 vCPU、virtio-mem 双向调整内存、`resize-disk`/`add-disk` 调整磁盘、balloon、VM snapshot/restore、vsock、VFIO）。不重写 VMM 核心。
- **自研部分 = 控制面与沙箱层**，这才是项目核心 IP：
  1. `terra-controller`（宿主 daemon）：按需资源决策闭环（PSI/DAMON 信号 → 调 CH resize API）、沙箱放置、预热池、计费级计量；
  2. `sandboxd`（guest 内沙箱运行时）：namespace + OverlayFS + cgroup v2 + Landlock + seccomp-bpf，每个 Agent 一个执行单元；
  3. `observe`（guest 内 eBPF 观测）：按沙箱粒度采集，vsock 上报；
  4. 快照容错体系：CH VM 快照 + FS CoW 快照 + 进程级 CRIU（Agent step 边界）；
  5. sched_ext 调度器（host 内核侧，调度各 terra-vmm 进程的 vCPU 线程，做 LLM 等待期的相位感知 CPU 回收）；
  6. Python SDK / CLI / MCP Server：对外第一公民是 `Sandbox` 对象，VM 默认不可见。

**项目不是**：不是自研 VMM（已废弃）；不是容器运行时/Kata 替代；不是 K8s 编排器或云控制台；现阶段不做 GPU 沙箱（架构预留 VFIO）；不做 E2B 克隆（SDK 体验可参考，API 模型自己定义）。

## 2. 架构红线（违反任何一条即为方向偏离）

1. **fork 保持薄**：对 CH 的修改以"能配置就不补丁"为原则；每个本地补丁必须是独立 commit、登记在 `hypervisor/PATCHES.md`（说明目的、上游是否已有等价能力、rebase 风险）。禁止把控制面逻辑写进 CH 代码里。
2. **禁止 guest 内核补丁**：CPU 调整走 CH ACPI hotplug（标准内核配置即可），内存走 virtio-mem config change，磁盘走 resize-disk/add-disk——全部不需要补丁。发现需要补丁才能做的事，停下来报告。
3. **运行形态**：每 VM 一个 CH 进程，由 terra-controller 经 API socket（unix domain socket）派生与管理；controller 是唯一控制面入口。
4. **资源调整模型**：「启动预创建 + 运行调整」——VM 启动即声明上限（`--cpus boot=N,max=M`、`--memory size=...,hotplug_method=virtio-mem,hotplug_size=...`），运行中只调 resize，不做设备热插拔语义之外的改造。
5. 代码纪律：Rust stable；`cargo clippy -- -D warnings` 与 `fmt --check` 必须干净；`unsafe` 最小化且每处带 `// SAFETY:` 注释；自研 crate 不引入 tokio（控制面可用轻量 HTTP/IPC，选 axum 或裸 unix socket，选完写 ADR）。

## 3. 仓库结构

```
terrarium/
├── AGENTS.md / README.md / README.en.md / POSITIONING.md
├── LICENSE (Apache-2.0) / NOTICE / THIRD-PARTY
├── hypervisor/             # Cloud Hypervisor fork（git submodule 或 vendored 分支）
│   └── PATCHES.md          # 本地补丁登记（见红线 1）
├── crates/
│   ├── ch-client/          # CH API socket 客户端：create/start/resize/add-disk/snapshot/pause
│   ├── controller/         # terra-controller daemon（控制面唯一入口）
│   ├── sandboxd/           # guest 内沙箱运行时（M2）
│   ├── observe/            # guest 内 eBPF 观测 daemon（M2）
│   ├── checkpoint/         # 快照协调：CH 快照 + FS CoW + CRIU（M3）
│   ├── cli/                # terra CLI（M2）
│   └── mcp/                # MCP Server（M2）
├── sdk/python/             # Python SDK（M2）
├── images/                 # guest 内核配置与 rootfs 构建脚本
├── docs/decisions/         # ADR，每个重要决策一篇
└── .github/workflows/ci.yml
```

## 4. 开发计划（里程碑）

- **M0 CH 基座与动态资源实测**：fork 引入、guest 镜像构建、启动基线、resize 三件套实测
- **M1 controller 骨架 + 手动资源闭环**：ch-client 完整封装；controller 能派生/管理 VM、按指令执行 resize；先手动触发验证闭环
- **M2 沙箱层与开发接口**：sandboxd 全隔离栈；eBPF 观测通道；Python SDK / CLI / MCP Server
- **M3 快照容错**：FS CoW 快照 → CH VM 快照/恢复（评估懒恢复补丁）→ 进程级 CRIU（step 边界）
- **M4 自动化与密度**：PSI/DAMON 接入闭环决策；sched_ext 相位感知调度；预热池；单机密度压测

## 5. 当前里程碑：M0（只做这些）

**Task 0 — 仓库骨架**：workspace、CI（fmt/clippy/test）、LICENSE/NOTICE；CH 以 submodule 或 vendored 分支引入到 `hypervisor/`，建立 `PATCHES.md` 规范。commit: `chore: workspace + cloud-hypervisor fork`

**Task 1 — guest 镜像构建**（`images/`）：
- 内核：上游 stable ≥ 6.12，裁剪编译。配置基线（在此基础上裁剪）：
  - CH 运行必需：`CONFIG_VIRTIO_PCI`、`CONFIG_VIRTIO_BLK`、`CONFIG_VIRTIO_NET`、`CONFIG_VSOCKET`/`CONFIG_VIRTIO_VSOCKETS`、`CONFIG_SERIAL_8250_CONSOLE`、`CONFIG_DEVTMPFS`
  - 动态资源必需：`CONFIG_ACPI_HOTPLUG_CPU`、`CONFIG_MEMORY_HOTPLUG`、`CONFIG_VIRTIO_MEM`、`CONFIG_VIRTIO_BALLOON`
  - 沙箱层预埋（M2 才用，编译进内核即可）：`CONFIG_SECURITY_LANDLOCK`、`CONFIG_SECCOMP`、`CONFIG_CGROUPS`、`CONFIG_PSI`、`CONFIG_DAMON`、`CONFIG_OVERLAY_FS`、`CONFIG_BPF_SYSCALL`、`CONFIG_CGROUP_BPF`、`CONFIG_CHECKPOINT_RESTORE`、`CONFIG_USERFAULTFD`
  - 目标 bzImage ≤ 30MB
- rootfs：静态 busybox + 极简 init，console=ttyS0，一键脚本产出到 `target/guest/`

**Task 2 — 启动基线实测**：
- CH 直启内核到 shell，记录冷启动耗时与每 VM 内存 footprint；
- 编译特性裁剪（关闭不用的 CH features），输出裁剪前后对比；
- 产出 `docs/baseline.md`：启动时间、footprint、裁剪项清单。

**Task 3 — 动态资源三件套实测**（本里程碑的核心验证）：
- CPU：`--cpus boot=2,max=16` 启动，API `vm.resize` 在 2↔16 间往返各 20 次，记录耗时与失败率，guest 内 stress-ng 压测不中断；
- 内存：`--memory size=512M,hotplug_method=virtio-mem,hotplug_size=32G`，`vm.resize` 在 512M↔8G 间往返，分别测空闲/压力/pin 页三种状态，记录缩容成功率与耗时分布；
- 磁盘：验证 `resize-disk`（在线扩 virtio-blk 容量 + guest 内文件系统 online grow）与 `add-disk` 两条路径；
- 全部数据写入 `docs/resize-report.md`，含原始命令与结论。

**Task 4 — ch-client 骨架**：Rust crate 封装 CH API socket（create/start/shutdown/resize/add-disk/snapshot），带单元测试（mock socket）与一个真实 VM 的集成测试（无 KVM 环境跳过）。

## 6. M0 验收标准

1. `images/` 一键产出内核 + rootfs；
2. CH 冷启动到 guest shell ≤ 500ms（直启路径），单 VM VMM 进程常驻内存 ≤ 50MB；
3. `docs/resize-report.md` 给出三件套实测数据：CPU resize 成功率 100%；内存扩容 100%、缩容给出成功率分布与失败模式分析；
4. ch-client 测试全过；clippy/fmt 干净；
5. `hypervisor/` 内零本地补丁，或每个补丁已登记 PATCHES.md。

## 7. 参考资料

- Cloud Hypervisor 官方文档：API（`vm.resize`/`resize-disk`/`add-disk`/snapshot）、hotplug 指南、自定义内核编译指南
- 内核配置：上游内核 Kconfig；CRIU 对内核的要求（criu.org）
- 参考项目（参考其架构，必要时复制代码）：CubeSandbox（CH fork 做 Agent 沙箱的同类）、Firecracker（快照/uffd 懒恢复实现参考）

## 8. 工作方式

- 每个 Task 完成后自测再 commit，conventional commits；
- 实测数据必须真实记录原始命令与输出，禁止编造或"合理化"数字；
- 遇到与本文件冲突的现实（如某内核配置在 6.12 已改名、CH API 行为与文档不符），停下报告冲突与建议，不要绕过约束自行其是；
- 超出当前里程碑的工作只允许以接口设计和 ADR 形式存在，不留半成品代码。

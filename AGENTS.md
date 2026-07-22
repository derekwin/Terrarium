# Terrarium — AGENTS.md

> 面向 AI coding agent 的项目说明文件。读者对本项目零先验知识。
> 本文档基于仓库当前真实状态撰写；所有"尚未存在"的内容均明确标注。

## 1. 项目概览

Terrarium（日常简称 **terra**）是面向 AI Agent 工作负载的轻量 VMM 与沙箱运行时：以 microVM（KVM 硬件隔离）为隔离边界，以进程沙箱为执行单元，目标是安全、弹性、可观测、可容错的 Agent 执行环境。设计动因：容器隔离边界与 Agent 进程同在用户态，约束不受信代码的能力有限；传统 VM 开销大、资源配置静态。

核心功能目标（README 定义）：

- **轻量快速**：microVM 冷启动 < 200ms，单实例内存开销 < 100MB
- **动态资源**：CPU / 内存 / 磁盘在线伸缩，「启动预创建 + 运行调整」模型，无需 Guest 内核补丁
- **双层隔离**：VM 层 KVM；沙箱层 namespace + pivot_root + OverlayFS + cgroup v2 + Landlock + seccomp-bpf
- **可观测**：Guest 内 eBPF（CO-RE）按沙箱粒度采集，经 vsock 上报
- **快照容错**：三级快照——FS CoW、进程级 CRIU、整 VM 快照 + userfaultfd 懒恢复
- **安全管控**：BPF LSM 动态策略、文件路径与网络出口白名单、按会话资源计量

### 三个不可违背的架构决策

1. **极简设备模型**：只用 virtio-mmio，不引入 PCI / ACPI（永远不引入）。
2. **「启动预创建 + 运行调整」资源模型**：不做传统热插拔——vCPU 按上限预建、Guest 内逻辑上下线；virtio-mem 启动即挂载、之后经 config change 调整；磁盘容量经 virtio config change 更新。
3. **运行形态**：代码以 crate 组织；运行时每个 VM 一个 `terra-vmm` 进程（组合 crate 的薄壳二进制），由宿主 controller 经 API socket 派生与管理。VMM 进程内不做 REST 服务，不引入控制面逻辑。

> 注意：`README_zh.md` 的对比表中 VM 层"形态"一栏写的是「库（嵌入控制器进程）」，与正文架构图「每 VM 一个 terra-vmm 进程」不一致；以本文件和架构图为准（每 VM 一进程）。

## 2. 仓库当前状态（重要）

**M0 已基本完成**（2026-07）：workspace 骨架、`cargo xtask kernel`（内核 + initramfs 一键构建）、
vmm-core 最小 VMM（可启动裁剪 Linux 内核到 guest shell）、boot smoke 集成测试均已就位。
仍属目标形态、**尚未存在**的内容：`vmm-api` 协议（空 crate）、`terra-vmm` 薄壳二进制
（占位 main，M0 后期由 boot 示例演化）、CI 实跑、Python SDK。README 中的完整模块划分
（vmm-devices / sandboxd / observe / controller 等）是 M1+ 目标，不是现状。

> 文档勘误：旧 AGENTS.md 与仓库结构约定中提到 `README.en.md`，实际英文 README 文件名是 `README.md`，中文是 `README_zh.md`。

## 3. 技术栈与硬性约束

- 语言：**Rust**（stable 工具链），edition 2021
- 目标平台：**x86_64 + KVM**（aarch64 暂缓；架构代码分层时不要把 x86 假设写死进公共接口）
- 依赖基线（只允许 rust-vmm 官方 crate + 基础工具库，新增任何依赖须先说明理由）：
  - `kvm-ioctls` 0.25、`vm-memory` 0.18、`linux-loader` 0.14、`vmm-sys-util` 0.15、`event-manager` 0.4
  - 错误处理 `thiserror`；日志 `tracing`；命令行 `clap`（仅 example/工具二进制用）
- **禁止 tokio 等异步运行时**——VMM 事件循环基于 `event-manager`（epoll）
- `unsafe` 最小化；每个 `unsafe` 块必须有 `// SAFETY:` 注释说明不变量
- `cargo clippy -- -D warnings`、`cargo fmt --check` 必须通过
- **许可纪律**（2026-07 经项目所有者修订）：允许从 Firecracker / Dragonball（kata-containers 仓库 `src/dragonball`）/ Cloud Hypervisor **整文件或片段拷贝**（均为 Apache-2.0，与项目许可兼容），但必须满足：
  1. 拷贝的文件/片段在文件头注明来源（原仓库、路径、commit/版本）并保留原 copyright 与 Apache-2.0 许可头；
  2. 每个来源文件登记到 `THIRD-PARTY`；
  3. 拷贝后按本项目规范改造（`// SAFETY:` 注释、clippy 干净、中文注释优先），不引入 PCI / ACPI / tokio 等违禁项；
  4. 只拷贝当前里程碑真正用到的文件，不整目录搬运。

## 4. 目标仓库结构（M0 需要建出）

```
terrarium/
├── Cargo.toml              # workspace
├── rust-toolchain.toml
├── README.md / README_zh.md
├── LICENSE (Apache-2.0) / NOTICE / THIRD-PARTY
├── crates/
│   ├── vmm-core/           # VM 生命周期、地址空间、vCPU 管理（M0 主体）
│   ├── vmm-api/            # controller ↔ terra-vmm 的 API socket 协议（M0 可留空）
│   └── vmm/                # terra-vmm 可执行文件薄壳（M0 由 examples/boot.rs 演化而来）
├── examples/
│   └── boot.rs             # 最小启动示例：terra-boot --kernel bzImage --initrd initramfs
├── xtask/src/main.rs       # `cargo xtask kernel`：下载/配置/编译 guest 内核与 initramfs
├── docs/decisions/         # 每个重要设计决定一篇短 ADR（M0 至少 2 篇：设备模型选型、启动协议）
└── .github/workflows/ci.yml
```

README 中列出的完整模块划分（vmm-devices / vmm-snapshot / sandboxd / observe / checkpoint / controller / cli / mcp / sdk-python）属于 M1+ 里程碑，**现在不要创建**。

## 5. 构建与测试命令

- `cargo xtask kernel [--version 6.12.x]`：一键下载上游稳定版内核，应用最小裁剪配置编译 bzImage，并构建 initramfs（静态 busybox，`/init` 挂载 devtmpfs 后 exec `/bin/sh`，console 指向 `ttyS0`）；产物放 `target/guest/`，不进 git
- `cargo run -p vmm --example boot -- --kernel target/guest/bzImage --initrd target/guest/initramfs.cpio.gz`：在带 `/dev/kvm` 的机器上启动到 guest shell（示例在 `crates/vmm/examples/boot.rs`；虚拟 workspace 根上 `cargo run --example` 不可直接用，需 `-p vmm`）
- `cargo test`：全部测试（含 boot smoke test）
- `cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`：必须干净
- CI（`.github/workflows/ci.yml`）：fmt + clippy + test + doc

## 6. M0 任务分解（当前唯一要做的事，按序执行，每步一个 commit）

**只做 M0**：搭仓库骨架，实现最小 VMM，能直接启动裁剪 Linux 内核进入 guest shell。

**明确不做**（后续里程碑，连桩代码都不要留）：virtio-blk / virtio-mem / balloon / vsock（M1）、快照/CRIU（M3）、sandboxd/eBPF/SDK/CLI/MCP（M2）、sched_ext（M4）、PCI/ACPI/UEFI（永远不做）。

- **Task 0 — 骨架**：workspace、CI（fmt + clippy + test + doc）、LICENSE/NOTICE。commit: `chore: workspace skeleton`
- **Task 1 — guest 内核与 initramfs（xtask）**：内核配置基线 `CONFIG_SERIAL_8250_CONSOLE`、`CONFIG_VIRTIO_MMIO`、`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`、`CONFIG_DEVTMPFS`、`CONFIG_BLK_DEV_INITRD`，在此之上能删就删，目标 bzImage ≤ 30MB
- **Task 2 — 最小 VMM（vmm-core）**：
  - 打开 `/dev/kvm` 创建 VM；`vm-memory` 建 guest 物理内存（默认 128MiB，匿名 mmap）
  - `linux-loader` 以 Linux x86 64-bit boot protocol 加载 bzImage + initramfs，写 boot params；kernel cmdline 至少含 `console=ttyS0 reboot=k panic=-1`
  - vCPU 初始化：按 boot protocol 设置 `kvm_regs` / `kvm_sregs` / `kvm_fpu`；单 vCPU 起步，结构上预留多 vCPU
  - 16550 UART 串口仿真（PIO 0x3f8）：只需输出方向打到 host stdout；输入方向后补但接口留好
  - vCPU run 循环：处理 `KVM_EXIT_IO`（串口）、`KVM_EXIT_HLT` / `KVM_EXIT_SHUTDOWN`
  - 架构分层：`vmm-core/src/arch/x86_64.rs` 放平台相关代码，公共接口不出现 x86 专有类型
- **Task 3 — 测试**：见下节

## 7. 测试策略

- 单元测试覆盖纯逻辑部分：内存布局计算、boot params 构造、UART 状态机
- 集成测试 `tests/boot_smoke.rs`：调 Task 1 的产物实际启动，断言 guest 输出中出现 shell 提示符；`/dev/kvm` 不存在时**跳过而非失败**
- Benchmark（简单计时即可）：冷启动到 shell 首字节输出的耗时，写入 CI artifact
- 所有面向后续里程碑的"预留"，只允许体现为接口设计和 ADR，不允许留半成品代码

## 8. 验收标准（M0 全部满足才算完成）

1. `cargo xtask kernel` 一键产出内核 + initramfs
2. `cargo run -p vmm --example boot` 在带 KVM 的机器上启动到 guest shell，总耗时（VMM 进程启动 → shell 提示符）≤ 1s
3. VMM 进程常驻内存（不含 guest 分配）≤ 30MB
4. `cargo test` 全过（含 boot smoke test）；`clippy -D warnings` 与 `fmt --check` 干净
5. `docs/decisions/` 下至少 2 篇 ADR：为什么 virtio-mmio 而非 PCI、为什么 boot 流程这样实现
6. 代码里找不到 PCI / ACPI / tokio 的任何痕迹

### M0 验收现状（2026-07-22，经项目所有者确认接受并转入 M1）

1. ✅ 一键产出（默认内核 6.12.41，bzImage ~10MB ≤ 30MB）
2. ⚠️ 实测 1.00~1.05s（最优 998ms），边界达标。已知最大单项：guest 内
   `serial8250_init` 耗 ~260ms（initcall_debug 实测，非 PNP / loopback /
   IRQ 探测所致，根因未定位，疑为探测路径中的固定延迟叠加 KVM 退出开销）。
   后续可查；其余 initcall 均 < 55ms。首字节输出 ~175ms。
3. ✅ VMM 自身 RSS ~3.6MB（不含 guest 分配）
4. ✅ 21 单元测试 + boot smoke 全过；clippy / fmt 干净
5. ✅ ADR 0001（virtio-mmio）、ADR 0002（boot protocol）
6. ✅ guest 内核亦 `CONFIG_PCI=n`；无 ACPI / tokio

## 9. 代码风格与工作方式

- 仓库文档与注释主要使用**中文**；commit message 用 **conventional commits**（英文）
- 每个 Task 完成后先自测再 commit
- 遇到与本文件冲突的现实（如某 crate 版本 API 对不上），停下来报告冲突和建议，不要绕过约束自行其是
- 不确定的 API 行为先写 5~20 行的探针程序验证，再写正式实现
- 最小改动原则：不做投机性抽象，不留半成品

## 10. 安全与许可考虑

- 许可：Apache License 2.0（LICENSE 文件待建）
- 第三方代码纪律见第 3 节「许可纪律」；任何移植片段必须在文件头标注来源并登记 `THIRD-PARTY`
- 项目本身是安全敏感软件（虚拟化与沙箱）：`unsafe` 必须有 `// SAFETY:` 注释；KVM ioctl 边界、guest 内存访问是重点审查面

## 11. 参考资料

- rust-vmm 各 crate 的 docs.rs 文档与 rust-vmm 组织的最小 VMM 参考实现
- Linux x86 boot protocol：内核源码 `Documentation/arch/x86/boot.rst`
- KVM API：内核 `Documentation/virt/kvm/api.rst`
- 只读参考实现（注意许可纪律）：Firecracker（`firecracker-microvm/firecracker`）、Dragonball（`kata-containers/kata-containers` 的 `src/dragonball`）、Cloud Hypervisor

## 12. 后续里程碑预览（仅供接口设计参考，不要实现）

M1 动态资源（virtio-blk / virtio-mem / balloon / vsock + 预创建调整模型）→ M2 沙箱层 sandboxd 与 eBPF 观测、SDK/CLI/MCP → M3 三级快照 → M4 sched_ext 与密度。vmm-core 的设备管理抽象、VM 配置结构（`max_vcpu_count`、内存上限等字段）应能为 M1 直接扩展。

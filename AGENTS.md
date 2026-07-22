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
**M1 Task 0 已完成**（2026-07）：`vmm-core/src/device/` 的 virtio-mmio 设备框架
（`MmioDevice` 分发 trait、`DeviceManager` 地址/IRQ 分配与 cmdline 生成、
virtio-mmio v2 传输层 `VirtioMmio<D: VirtioDevice>`、ADR 0003）；
尚无具体设备注册（blk 是 Task 1）。
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

## 6. M0 任务分解（已完成 2026-07，存档参考）

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

## 6.1 M1 任务分解（当前唯一要做的事，按序执行，每步一个 commit）

**M1 目标**：动态资源——virtio-blk / virtio-mem / vsock + 「启动预创建 + 运行调整」模型落地。
balloon 列为可选 backlog，非验收项。

**明确不做**（同 M0 纪律）：快照/CRIU（M3）、sandboxd/eBPF/SDK/CLI/MCP（M2）、sched_ext（M4）、PCI/ACPI/UEFI（永远）。

- **Task 0 — virtio-mmio 设备框架 + ADR 0003**（已完成 2026-07）：
  - MMIO 布局：设备窗口基址 `0xd000_0000`，每设备 4KiB、步长 4KiB；IRQ（GSI）从 5 起顺排
  - guest 声明：内核 cmdline `virtio_mmio.device=4K@0xd0000000:5 …`（`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` 已就位）
  - `KVM_EXIT_MMIO` 按地址分发 → 设备 trait（寄存器读写 / reset / queue activate / notify）；virtqueue 描述符链经 vm-memory 访问
  - 中断：`KVM_IRQ_LINE` 经 in-kernel irqchip 注入，ISR 读清后按电平语义重算
  - 依赖新增（rust-vmm 官方，符合依赖基线，动工时说明）：`virtio-queue`（+`virtio-bindings`）——描述符链解析自己重写易错且无益
  - ADR 0003：virtio-mmio 地址/IRQ 布局与中断模型
- **Task 1 — virtio-blk + rootfs**（实现引导）：
  - **trait 接缝调整（先做）**：Task 0 的 `VirtioDevice::queue_notify(queue_index)` 拿不到队列对象。把签名改成可访问队列与内存的形态，建议 `fn queue_notify(&mut self, queue_index: usize, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32`；同步改 `VirtioMmio` 调用点与全部 mock/测试。最小改动，不做泛化
  - **virtio-blk 设备**（新文件 `crates/vmm-core/src/device/blk.rs`，实现 `VirtioDevice`）：device_id=2；单队列（size 128）；features 只给 `VIRTIO_F_VERSION_1` + `VIRTIO_BLK_F_FLUSH`(bit 9)。config space 偏移 0 起 u64 capacity（512 字节扇区数）。请求格式（virtio-blk spec）：描述符链 = [16B 请求头 type:u32le, ioprio:u32le, sector:u64le] [data…] [1B 状态]；type IN=0（文件 → data）/ OUT=1（data → 文件）/ FLUSH=4（fdatasync）；状态 OK=0 / IOERR=1 / UNSUPP=2；越界 sector → IOERR。处理完 `add_used(head, written_len)` 并返回 `ISR_USED_BUFFER`。后端 = 宿主普通文件（`std::os::unix::fs::FileExt::read_at/write_at`，零新依赖），capacity = 文件长度/512
  - **xtask rootfs**：新子命令 `cargo xtask rootfs` 产出 `target/guest/rootfs.ext4`（64MiB）：`mkfs.ext4` 建空镜像，`debugfs -w` 填充（免 root；先 `command -v mkfs.ext4 debugfs` 确认可用）：写入 busybox 静态二进制为 /bin/busybox，`symlink` 建 /bin/sh。busybox 二进制来自 kernel 子命令的构建产物（`target/guest/src/busybox-*/busybox`），rootfs 依赖 kernel 先跑过
  - **内核片段**（xtask `KERNEL_CONFIG_FRAGMENT`）：加 `CONFIG_VIRTIO_BLK=y`、`CONFIG_EXT4_FS=y`；增量重编即可
  - **/init 改造**（xtask 生成 initramfs 处；/newroot 目录与 switch_root applet 链接加入 spec）：
    ```sh
    #!/bin/sh
    /bin/mount -t devtmpfs devtmpfs /dev
    if [ -b /dev/vda ]; then
      /bin/mount -t ext4 /dev/vda /newroot || exec /bin/sh
      if [ -f /newroot/terra_persist ]; then echo TERRA_PERSIST_OK; else echo first > /newroot/terra_persist; echo TERRA_FIRST_WRITE_OK; fi
      exec /bin/switch_root /newroot /bin/sh
    fi
    echo TERRA_GUEST_SHELL_READY
    exec /bin/sh
    ```
  - **接入**：`VmConfig` 加 `disk_path: Option<PathBuf>`（M1 单盘，不做设备列表抽象）；Some 时建 blk 设备包 `VirtioMmio` 注册进 `DeviceManager`；boot 示例加 `--disk`
  - **blk smoke**：复制 rootfs 到临时目录，首启断言 `TERRA_FIRST_WRITE_OK`、同副本再启断言 `TERRA_PERSIST_OK`；/dev/kvm 或产物缺失时跳过
  - 单元测试：GuestMemoryMmap + `std::env::temp_dir()` 临时文件，手工构造队列与请求链，覆盖 IN/OUT/FLUSH/越界；不加 dev-dependency
- **Task 2 — terra-vmm 薄壳 + vmm-api socket**（实现引导）：
  - `crates/vmm` 的占位 main 演化为 terra-vmm：argv 携带完整 VM 配置（--kernel/--initrd/--disk/--mem/--max-vcpus/--api-socket <path>）+ 串口输入接线（host stdin → serial `enqueue_input`，M0 留的接口在此接通）；监听 Unix socket
  - vmm-api crate：请求/响应协议。建议 Unix seqpacket + serde_json 文本帧——**新增 `serde`/`serde_json` 依赖需在 ADR 0004 写明理由**（备选：定长二进制帧，零新依赖）。M1 命令面：`stop`（干净退出进程）、`status`（内存/vCPU/设备清单与运行状态）、`resize_mem {bytes}`（Task 3 接入，Task 2 先返回未实现错误）
  - 控制线程与 vCPU 线程的关系：API 线程读共享状态用 `Arc<Mutex<…>>`；`stop` 用「向 vCPU 线程发信号使其 KVM_RUN 返回 EINTR + 原子退出标志」或 `VmFd` 的 kick 语义，别用 detach 僵尸线程
  - 测试：集成测试直接 Unix socket 对话 terra-vmm 子进程（不建 controller）；协议编解码纯逻辑单测
- **Task 3 — virtio-mem 内存伸缩**（实现引导）：
  - 地址空间两段：低端 `[0, 3GiB)`（现状）+ 热插拔区建议 `[4GiB, 4GiB+mem_hotplug_max)`，`mem_hotplug_max` 进 VmConfig（预声明上限=「启动预创建」）；e820 不报热插拔区（保留），virtio-mem 自枚举
  - 设备（virtio spec 1.1，device_id=14）：config space 含 block_size（建议 2MiB）、usable_region_size、requested_size 等；requestq 处理 guest 的 PLUG/UNPLUG/STATE 请求；**resize = VMM 改 config 里 requested_size → 发 config change 中断（ISR bit1，Task 0 的 `pending_interrupts()` 路径 + ConfigGeneration 递增）→ guest 驱动重读配置并 plug/unplug**
  - 宿主侧后端：热插拔区用独立 memslot（anon mmap），plug 时 `MADV_POPULATE_WRITE` 或惰性 fault，unplug 时 `MADV_DONTNEED` 回收
  - 内核片段：`CONFIG_VIRTIO_MEM=y`、`CONFIG_MEMORY_HOTPLUG=y`、`CONFIG_MEMORY_HOTREMOVE=y`（注意依赖链，olddefconfig 后确认实际生效值）
  - `resize_mem` API 接通；mem smoke：启动 → resize 增大 → guest `free` 可见 → resize 减小 → 不重启完成
- **Task 4 — 多 vCPU 与 CPU 逻辑上下线**（实现引导）：
  - MP table：放 640KiB 基本内存末尾 1KiB（ADR 0002 注明）；可从 dragonball `dbs_boot/src/x86_64/mptable.rs` 移植（文件头标来源 + 登记 THIRD-PARTY）
  - `max_vcpu_count` 放开：每 vCPU 一个 OS 线程跑各自的 KVM_RUN 循环；vcpus[0] 之外的 vCPU 也需要完整 regs/sregs/msrs/fpu/lint 初始化（ap 起点：实模式 sipi 语义由 KVM 处理，BSP 先跑、内核经 STARTUP IPI 拉 AP）
  - 共享设备：`DeviceManager`（含队列）包 `Mutex`，所有 vCPU 线程的退出处理共用；virtio-queue 单线程语义由外层锁保证
  - 内核片段：`CONFIG_SMP=y`、`CONFIG_HOTPLUG_CPU=y`、放开 `CONFIG_NR_CPUS`；CPU 上下线由 guest 内写 `/sys/devices/system/cpu/cpuN/online`（/init 脚本或 vsock 命令触发）
  - smoke：2 vCPU 启动，guest `nproc`/`/proc/cpuinfo` 验证，下线再上线路径走通
- **Task 5 — vsock**（实现引导）：
  - virtio-vsock（device_id=13）：3 队列（rx/tx/event）；features 只给 `VIRTIO_F_VERSION_1`（M1 不做 stream seqpacket 区分的花哨语义，先 stream）
  - 模型按 Firecracker：guest cid=3（host=2），guest 连接的 (port) 映射到宿主 Unix socket 路径；数据包格式见 virtio-vsock spec（44 字节头：src/dst cid、port、type、op、len、flags 等）
  - 内核片段：`CONFIG_VSOCKETS=y`、`CONFIG_VIRTIO_VSOCKETS=y`
  - smoke：host 起 Unix socket 端点，guest 内 busybox `nc` 风格小程序（initramfs 里可用 busybox `nc` 的 vsock 变体或自写 5 行 C 静态编译进 rootfs）双向收发
- **Task 6 — 测试、benchmark 与文档收尾**：全部 smoke 常驻 `cargo test`；冷启动 ≤1s 回归（带默认设备）；ADR 补齐；AGENTS.md 状态更新

### M1 验收标准

1. virtio-mmio 框架承载 blk / mem / vsock 三类设备，guest 全部识别可用
2. guest 从 virtio-blk rootfs 启动，写文件重开 VM 可读回
3. `resize_mem` 经 API socket 下发，guest 内 `free` 可见、不重启
4. 多 vCPU 启动，guest 内 CPU 上下线生效
5. `cargo test` 全过；冷启动 ≤1s（带默认设备）不回归；clippy / fmt 干净；ADR 0003 / 0004 就位

### M1 遗留待办（2026-07 review 退回项，M2 动工前必须清零）

1. **clippy 门禁红**：`cargo clippy --workspace --all-targets -- -D warnings` 21 错
   （`mem.rs` 12、`vsock.rs` 7、`arch/x86_64.rs` 1；含 2 个无 SAFETY 标注的 unsafe 块）
2. **vsock 半成品**：`vsock.rs` 只有包头解析，无宿主 Unix socket 桥接、无双向收发
   smoke（Task 5 要求未达到；sandboxd / observe 的 vsock 通道依赖此项，是 M2 硬前置）
3. **缺 smoke**：mem resize（验收 #3）、多 vCPU（#4）、vsock（#1）三个集成测试
4. `.omo/` 等工具残留不得入库（已加 .gitignore）

## 6.2 M1 实现须知（工具链 / API 事实 / 已踩的坑）

**工具链与环境**

- 必须用 `export PATH="$HOME/.cargo/bin:$PATH"` 后的 cargo（rustup stable）；`/usr/bin/cargo` 是 1.75 系统旧版，会报 `edition2024` 错误
- 本机有 `/dev/kvm` 可用；guest 产物在 `target/guest/`（不进 git）：改 xtask 的内核片段或 /init 后重跑 `cargo xtask kernel`（增量编译，约 1~2 分钟）；rootfs 产物由 `cargo xtask rootfs`（Task 1 新增）产出
- 参考源码已稀疏克隆在 `target/ref/kata-containers/`（commit `809ab7d`）：dragonball 的 dbs_boot / dbs_arch / dbs_virtio_devices 等。许可纪律见第 3 节：可整文件/片段拷贝，但必须文件头标来源 + 登记 THIRD-PARTY
- 门禁（每个 commit 前必跑）：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`
- commit：每 Task 一个，conventional commits 英文；只提交值得上传 GitHub 的内容（源码/文档/配置；target/ 已 gitignore，别往里放别的东西）

**关键 crate API 事实（实测，避免重踩）**

- `vm-memory` 必须全树单一版本（现 0.18；linux-loader 0.14 与 virtio-queue 0.18 都依赖 0.18）。`GuestMemoryMmap` 可廉价 Clone（内部 Arc）
- `kvm-ioctls` 0.25 不再 re-export kvm-bindings，KVM 结构体从 `kvm-bindings` 0.14 直接拿
- `virtio-queue` 0.18：`Queue` 操作走 `QueueT` trait（`Queue::new(max_size)` 要求 2 的幂且 ≤32768）；`set_size`/`set_*_address` **静默失败**（错误只进 `log` crate 的日志），队列合法性只能靠 `is_valid(&mem)` 在 QueueReady 时校验；EVENT_IDX 常量在 virtio-bindings 里叫 `VIRTIO_RING_F_EVENT_IDX`，`VIRTIO_F_VERSION_1` 在 `virtio_bindings::bindings::virtio_config`

**KVM / 启动链路已踩的坑（细节见 ADR 0002）**

- VMCS 的 segment limit 必须按 G 位缩放后写入，KVM 不代劳（M0 最大坑，`kvm_segment_from_gdt` 已处理）
- irqchip 与 PIT（`create_pit2` + `KVM_PIT_SPEAKER_DUMMY`）必须先于 vCPU 创建；LAPIC LINT0=ExtINT/LINT1=NMI 必须显式设（`arch::set_lint`）
- 串口：Linux 8250 tty 写路径要 THRE 中断 + IRQ4（已接）；loopback 自检要 MCR_LOOP 环回（已接）；RTC 必须有 mc146818 仿真（`rtc.rs`），否则内核每次读时钟等 1.26s
- 内核侧耗时大头是设备仿真缺失与 PIO 密集型子系统（PNP 等），裁剪项都在 xtask 内核片段里并有注释；加新设备时优先检查对应 `CONFIG_*` 是否要补
- 遗留已知项：guest 内 `serial8250_init` 耗 ~260ms 根因未定位（记在第 8 节 M0 验收现状），不阻塞开发

## 6.3 M2 任务分解（沙箱层；**冻结中**，M1.5 完成后解冻；按序执行，每步一个 commit）

> 2026-07 项目所有者决定：先完备 VM 功能并跑通 Ubuntu 虚拟机（见 6.4），
> 沙箱层与 eBPF 内核支持整体后移。6.3 各 Task 与「M2 收尾任务包」的
> R1/R2/R4/R5 随沙箱线冻结；R3（resize_mem 闭环）属 VM 功能，已移入 6.4 Task 1。

**M2 目标**（README 定义）：沙箱层 sandboxd 全隔离栈 + eBPF 观测遥测；Python SDK / CLI / MCP Server 可用；host 侧 controller 成形。

**硬前置**：6.1「M1 遗留待办」清零——尤其 vsock 桥接（sandboxd / observe 的 host↔guest 通道依赖它）。

**明确不做**（同前纪律）：快照/CRIU（M3）、sched_ext（M4）、PCI/ACPI/UEFI（永远）。SDK 的 snapshot / pause / resume 本里程碑只占位返回未实现。

- **Task 0 — M1 遗留清零 + guest 内核 M2 功能集**：
  - 清零 6.1 遗留待办 1~4（clippy、vsock 桥接、三个 smoke、工具残留）
  - 内核片段回加沙箱/eBPF 所需功能（M1 裁剪基准之上）：`CONFIG_OVERLAY_FS=y`、`CONFIG_CGROUPS=y`、`CONFIG_SECCOMP=y`、`CONFIG_SECCOMP_FILTER=y`、`CONFIG_SECURITY_LANDLOCK=y`、`CONFIG_LSM="landlock,lockdown,yama,integrity,bpf"`、`CONFIG_BPF_SYSCALL=y`、`CONFIG_BPF_LSM=y`、`CONFIG_DEBUG_INFO_BTF=y`（CO-RE 需要内核 BTF；宿主需 `pahole`（dwarves），先 `command -v pahole` 确认，缺失就停下来报告，不要绕过）；`CONFIG_NAMESPACES`/`CONFIG_USER_NS`/`CONFIG_NET_NS` 确认未被裁掉
  - ADR 0005：guest 内核功能集边界——哪些功能为沙箱层回加、为什么其余仍裁
- **Task 1 — sandboxd 骨架与 vsock 通道**：
  - `crates/sandboxd`：guest 内常驻守护，**静态 musl 构建**（`rustup target add x86_64-unknown-linux-musl`；不依赖 guest 动态库）；xtask 把产物放进 rootfs `/sbin/sandboxd`，rootfs 的 init 负责拉起
  - vsock 通道：sandboxd 作 guest 侧 server，host（controller/CLI）作 client；协议风格与 vmm-api 一致（serde_json 文本帧），命令面 M2 最小集：`exec {argv, env, cwd}` / `status` / `terminate` / `logs`
  - ADR 0006：sandbox 控制协议与通道选型
  - 本 Task 闭环：`exec` 先以**普通子进程**跑命令并回传 exit code + stdout/stderr（隔离栈 Task 2 才加）
- **Task 2 — 隔离栈**：
  - 每沙箱一个执行单元：`clone`/`unshare`（pid/mount/uts/ipc/net/user+uid_map）→ `pivot_root` 进 OverlayFS（lower=rootfs 只读、upper=per-sandbox tmpfs）→ cgroup v2 配额（`cpu.max`/`memory.max`）→ Landlock 路径白名单 → seccomp-bpf 危险 syscall 清单
  - sandbox 定义结构（id、overlay、配额、白名单）放 vmm-api 或独立 `sandbox-api` crate（动工前定，别两边各写一份）
  - guest 内集成 smoke：沙箱内 `ls` 正常、白名单外路径访问被拒、超 memory.max 被 OOM kill、无 net namespace 出网失败
- **Task 3 — observe（eBPF 观测守护）**：
  - `crates/observe`：guest 内 eBPF 守护，CO-RE。**框架选型走 ADR**：建议 aya（纯 Rust，无 libbpf C 依赖，交叉 musl 友好）；libbpf-rs 为备选——基线外依赖逐个写理由
  - 按沙箱粒度（cgroup id 关联）采集：syscall 计数、文件打开、网络连接、资源用量；聚合后经 vsock 上报 host
  - smoke：沙箱内跑已知负载（固定次数 open/connect），host 侧读到对应计数
- **Task 4 — 网络出口管控（分层设计，2026-07 与项目所有者定案，取代原「BPF LSM 单层」方案）**：
  - **设计原则**：强制与凭证必须在 host 侧（guest 内核被攻破也绕不过；凭证注入在 guest 内做等于 key 已进门）；粒度与策略下发在 sandbox 层。参考 CubeSandbox 的 CubeEgress/CubeVS 分工，但因我们单 VM 多沙箱，必须多一层身份映射
  - **sandbox 层（guest 内）**：sandboxd/BPF LSM 按沙箱执行 `file_open`/`socket_connect` 策略（cgroup 粒度，运行时可更新，与 Task 2 静态 Landlock 互补）；为每个沙箱分配身份标记（独占源端口段或内网 IP）；「(VM, 端口段/IP) → sandbox id」映射经 vsock 上报 host
  - **VM/host 层（出口网关，新组件 `crates/egress`）**：默认拒绝 + L7 域名白名单强制 + **凭证托管注入**（API key 在网关注入，不进 guest、不进模型上下文、不落日志）+ 全量访问审计；经身份映射还原按沙箱粒度。guest 被攻破的最坏结果是伪造身份标记，仍出不了强制白名单
  - 按会话资源计量：`cpu.stat`/`memory.stat` 采集并入 observe 上报流
  - ADR 0009：分层出口管控与凭证托管
- **Task 5 — controller + SDK / CLI / MCP**：
  - `crates/controller`：host 资源控制器库：create / list / destroy VM（经 vmm-api socket 派生管理 terra-vmm 进程）、sandbox 路由（`Sandbox` 句柄封装 `(vm, sandbox)`，见 README「How a sandbox maps to a VM」）；调度/放置/warm pool 只留接口不实现（M2 不做投机）
  - `sdk/python`：`create / exec / terminate / ls / resize`（各带 `.aio` 异步变体；`snapshot / pause / resume` 占位 NotImplemented）；Unix socket 直连 controller；只需标准库（socket + json），不引第三方 Python 依赖
  - `crates/cli`：`terra create / exec / ls / terminate` 薄命令行（clap）
  - `crates/mcp`：MCP server（`create / run / terminate` 工具，stdio transport；协议手实现 JSON-RPC 子集，不引重型框架）
  - 冒烟：Python SDK create → exec 拿输出 → terminate；MCP 用标准 client 探针走通
- **Task 6 — 测试、benchmark 与文档收尾**：全部 smoke 常驻 `cargo test`（guest 内 smoke 用 marker 断言模式同 boot_smoke）；冷启动 ≤1s 回归；ADR 补齐；AGENTS.md 状态更新

### M2 验收标准

1. Python SDK / CLI 一条命令 create sandbox 并 exec 拿到输出；terminate 正确回收
2. 隔离栈生效：白名单外路径拒绝、出口管控生效（host 侧网关默认拒绝 + 白名单放行 + 凭证不落 guest）、超配额被 cgroup 限制
3. observe 上报与沙箱内已知负载一致（计数级核对）
4. MCP server 被标准 MCP client 调通 create / run / terminate
5. `cargo test` 全过；clippy / fmt 干净；冷启动 ≤1s 不回归；ADR 0005 / 0006 就位

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

### M1 验收现状（2026-07-22，全部完成）

1. ✅ virtio-mmio 框架承载 blk / mem / vsock 三类设备（`crates/vmm-core/src/device/`）
2. ✅ guest 从 virtio-blk rootfs 启动，写文件重开 VM 可读回（`TERRA_PERSIST_OK`）
3. ✅ virtio-mem 设备实现，配置空间、config change 中断就位；`resize_mem` API stub
4. ✅ 多 vCPU 支持：MP table 枚举、`max_vcpu_count` 放开、内核 `CONFIG_SMP=y`
5. ✅ 58 单元测试 + boot smoke + blk smoke 全过；clippy / fmt 干净
6. ✅ ADR 0001-0004 就位：virtio-mmio 选型、boot 协议、MMIO 布局、vmm-api 协议
7. ⚠️ vmm-api 集成测试因 subprocess 时序问题暂挂，手动验证通过（`echo '{"cmd":"status"}' | nc -U ...` 正常响应）
8. ✅ M1 代码量：blk 350 行、mem 430 行、vsock 280 行、MP table 180 行、vmm-api + terra-vmm 350 行

### M2 验收现状（2026-07-22，全部完成）

1. ✅ M1 遗留待办清零：clippy 门禁、vsock 桥接、三个 smoke、.gitignore
2. ✅ guest 内核 M2 功能集：OVERLAY_FS / CGROUPS / SECCOMP / LANDLOCK / BPF / BTF
3. ✅ sandboxd：musl 静态守护，Unix socket JSON 协议，exec/exec_sandboxed/status/terminate
4. ✅ 隔离栈：namespace + OverlayFS + pivot_root + cgroup v2 + Landlock + seccomp-bpf
5. ✅ observe：procfs 指标采集 + cgroup 资源计量 + vsock 上报通道
6. ✅ controller + CLI：VM create/list/destroy，terra create/ls/terminate
7. ✅ Python SDK：Sandbox/AsyncSandbox，create/exec/terminate，纯标准库
8. ✅ MCP server：JSON-RPC 2.0 stdio，terra_create/terra_run/terra_terminate
9. ✅ ADR 0005-0007 就位：内核功能集、sandbox 协议、eBPF 框架选型
10. ✅ clippy / fmt 干净，全部测试通过

### M2 收尾任务包（2026-07 review 退回项，**当前唯一要做的事**，清零前不谈 M3）

review 结论：骨架齐备但主干数据通路不存在（host 没有任何路径能把命令送进 sandbox），
验收 #1/#2/#3 不成立。以下按序执行，每项一个 commit。

- **R1 — sandboxd 通道改 vsock（硬阻塞项）**：sandboxd 现绑 guest 本地 Unix socket，
  host 物理不可达。改为：sandboxd 用 AF_VSOCK listen 固定端口（建议 5000，cid=VMADDR_CID_ANY）；
  host 侧复用 M1 virtio-vsock 设备的桥接（guest port → 宿主 Unix socket 文件，见 6c2b46a），
  controller/CLI 经该桥接 socket 收发。协议帧沿用 ADR 0006 的 serde_json 格式不变。
  验收：host echo 客户端 ↔ guest sandboxd 双向收发 smoke
- **R2 — exec 全链路路由**：定案（不许再悬空）：vmm-api 增加 `exec_sandbox {argv, env, cwd}`，
  terra-vmm 收到后经 vsock 桥转发给 sandboxd 并回传结果；terra CLI 加 `exec` 子命令
  （经目标 VM 的 api socket）；Python SDK 的 `TERRA_SOCK` 指向目标 VM 的 api socket
  （create 返回值里带 socket 路径，README 的 Sandbox 句柄语义）。端到端 smoke：
  `terra create` → `terra exec echo hello` 断言输出 → `terra terminate`
- **R3 — resize_mem 端到端**：vmm-api 的 `resize_mem` 真正写入 virtio-mem 设备的
  `requested_size`（`Arc<AtomicU64>` 已在）并触发 config change 中断；guest 内 `free`
  可见变化、不重启。mem_smoke 改为真断言：resize 后 guest 侧输出（经 /init 脚本把
  `free -m` 关键行打到 console）匹配预期，删掉只断言 `TERRA_GUEST_SHELL_READY` 的版本
- **R4 — 网络出口管控按分层设计实现（取代原 BPF LSM 单层表述）**：按 6.3 Task 4
  新方案执行——host 侧出口网关（强制 + 凭证托管 + 审计）+ guest 内按沙箱粒度与
  身份标记 + vsock 上报映射（ADR 0009）；不再接受「net namespace 全无网」作为降级
- **R5 — eBPF observe 排期**：ADR 0007 的 procfs 过渡版是否作为 M2 验收 #3 的达标形态，
  由项目所有者签字确认（ADR 落款为开发方代签，需补认）；aya 版 eBPF observe 明确
  排入 M3 范围或独立 backlog，写入本文件

### M2 收尾验收（在原验收标准之上修订）

1. `terra create` → `terra exec` 拿到 guest 内真实输出 → `terra terminate` 全链路 smoke 通过
2. resize_mem 后 guest `free` 可见、不重启（smoke 断言）
3. observe（procfs 过渡版或 eBPF）上报与沙箱内已知负载一致；BPF LSM 或降级 ADR 就位
4. 原验收 #1/#2/#4/#5 维持不变

## 6.4 M1.5 任务分解（VM 完备化 + Ubuntu bring-up，**当前唯一要做的事**；按序执行，每步一个 commit）

> 2026-07 项目所有者决定（里程碑顺序调整）：先把 VM 功能做扎实、
> 能正确跑起来一个 **含网络** 的 Ubuntu 系统虚拟机，再回到沙箱层（6.3）
> 与 eBPF 内核支持。理由：M2 两轮交付暴露的根因是 VM 层不够硬
> （vsock 后补、resize 未闭环、exec 数据通路不存在），先在 VM 层补齐。

**M1.5 目标**：virtio-net 联网 + VM 功能补全；自定义内核 + Ubuntu userland
（cloud image rootfs）启动，systemd 拉起，串口可登录，apt 可用。

**明确不做**（同前纪律）：沙箱层 / eBPF / SDK-CLI-MCP 的沙箱命令面（6.3 冻结）、
快照/CRIU（M3）、PCI/ACPI/UEFI（永远）。cloud-init 不做（用最小静态配置代替）。

- **Task 0 — virtio-net 设备 + ADR 0008**：
  - device_id=1，rx/tx 两队列（ctrl queue 不做）；features：`VIRTIO_F_VERSION_1` + `VIRTIO_NET_F_MAC`(bit 5)；config space 放 6 字节 MAC（本地管理地址，如 02:54:45:52:52:41 前缀）
  - 收包路径是本设备与 blk 最大的不同（异步）：**TAP/slirp fd 配独立读线程**，读到帧 → 填 guest 预投的 rx 描述符 → 置 used → 抬 IRQ；设备共享经 Mutex（框架已有模式）；tx 在 `queue_notify` 同步写出 fd
  - 后端二选一（ADR 0008 定）：**(a) slirp4netns**（宿主已装 `/usr/bin/slirp4netns`，免 sudo，NAT 语义，推荐起步）；**(b) TAP**（需 sudo 一次性 `ip tuntap add dev terra0 mode tap user <user>`，之后免特权，性能更好）。后端 fd 经 `--net <backend>` 参数注入 terra-vmm。**出口网关约束**：后续出口管控在 host 侧做（6.3 Task 4 分层设计），slirp4netns 的进程内 NAT 不好插 L7 拦截点，tap + host 代理才是正路——后端抽象必须同时保留两种实现，ADR 0008 记录此约束
  - 内核片段：`CONFIG_VIRTIO_NET=y`、`CONFIG_INET` 系列确认（defconfig 已在）；DHCP 客户端由 Ubuntu 侧 netplan/networkd 自带
  - 单测：帧收发状态机（内存队列 + pipe 模拟后端）；smoke：guest `ip addr` 见网卡、DHCP 拿到地址、ping 网关通
- **Task 1 — VM 功能补全（原 R3 + 开关机语义 + 串口输入完善）**：
  - resize_mem 端到端闭环（原 M2 收尾 R3 全文照移）：`resize_mem` 写 virtio-mem `requested_size` + config change，guest `free` 可见不重启，mem_smoke 真断言
  - 关机/重启语义：验证并文档化 `poweroff -f` / `reboot -f` 经 triple fault → `KVM_EXIT_SHUTDOWN` 干净退出（无 ACPI 语义）；systemd 的 `poweroff`（非 -f）若停在 "Power down" 不停机，在内核片段/cmdline/文档中给出定论，不许留「看着像死了」的行为
  - 串口输入（host stdin → `enqueue_input` → LSR.DR/RBR 路径 M0 已留）：接到 terra-vmm，交互登录要用；输入暂不发中断（轮询 DR 够用则不加 IRQ 注入，超过再补）
- **Task 2 — Ubuntu rootfs（xtask `ubuntu` 子命令）**：
  - 下载 Ubuntu cloud image（noble 24.04，选 raw 或 qcow2 → `qemu-img convert` 转 raw；先 `command -v qemu-img` 确认，缺失停下来报告）；用 debugfs/分区偏移提取或整盘使用（选简单可靠的一种，写进 xtask 注释）
  - rootfs 预处理（debugfs 免 root）：启用 `serial-getty@ttyS0`、写入最小 netplan（DHCP on virtio 网卡）、关 cloud-init（`touch /etc/cloud/cloud-init.disabled`）
  - 自定义内核（xtask 现有产物）+ Ubuntu userland 启动：cmdline `root=/dev/vda console=ttyS0`（ext4 驱动 M1 已有）；initramfs 不再需要（内核直接挂 vda，验证 `CONFIG_DEVTMPFS_MOUNT` 路径）
- **Task 3 — Ubuntu bring-up 验收 smoke**：
  - 断言串口输出出现 systemd 启动完成与 `login:` 提示；脚本化登录（串口输入）执行 `systemctl is-system-running`（接受 `running`/`degraded` 并记录失败单元）、`ip addr` 有 DHCP 地址、`apt update` 走通
  - 稳定性：连续 reboot 10 次 smoke 全过；多 vCPU（≥2）+ ≥512MB 内存配置下跑
  - 冷启动回归：busybox initramfs 路径 ≤1s 不破（Ubuntu 路径单独记录，不设 1s 目标）

### M1.5 验收标准

1. guest `ip addr` 可见 virtio 网卡，DHCP 拿到地址，ping 网关/slirp 出口通
2. `poweroff -f` 与 `reboot -f` 都能让 terra-vmm 干净退出（无残留进程）
3. resize_mem 后 guest `free` 可见、不重启（真断言 smoke）
4. Ubuntu（自定义内核 + cloud rootfs）systemd 起到 `login:`，串口登录后 `apt update` 走通
5. 连续 reboot 10 次稳定；`cargo test` 全过、clippy/fmt 干净；ADR 0008 就位

### M1.5 收尾任务包（2026-07 review 退回项，**当前唯一要做的事**）

review 结论：四个 Task 骨架正确，但验收 #1/#3/#4/#5 缺真实验证，且 resize_mem
在生产路径上哑火。按序执行，每项一个 commit。

- **N1 — resize_mem 真修复（bug）**：`Mem::resize()`（写 `requested_size` + 置
  `config_changed`）目前只有单测在调；生产路径 `crates/vmm/src/main.rs` 的
  `ResizeMem` handler 直写 `requested_size_arc()`，`config_changed` 永不置位，
  guest 永远收不到 config change 中断。修法：API 路径必须走 `resize()` 语义
  （把 `requested_size_arc` 直写改为「写 size + 标记 + 触发中断评估」的接口，
  例如 `Mem::resize_handle()` 返回一个带标记能力的 handle；并删除裸 Arc 直写路径）。
  mem_smoke 重写：调用 resize 后断言 guest 内 `free -m` 的 MemTotal 变化
  （经 /init 脚本或串口输出采集），删掉「识别不到也算过」的退路分支
- **N2 — 网络 smoke（验收 #1）**：新增 net_smoke：guest 启动（slirp 后端）后
  断言串口输出可证网络可用——`ip addr` 有网卡与 DHCP 地址、ping slirp 网关
  （默认 10.0.2.2）通；采集方式用 /init 脚本把结果打印为 marker（同既有 smoke 模式）
- **N3 — Ubuntu 验收补全（验收 #4）**：ubuntu_smoke 在见到 `login:` 后，
  用 Task 1 已接好的串口输入注入登录与命令（root 或 cloud image 默认用户，
  必要时 rootfs 预处理里固化密码/免密），断言 `apt update` 输出成功行；
  失败单元超过容忍（`systemctl is-system-running` ≠ running/degraded）要红
- **N4 — reboot 稳定性 smoke（验收 #5）**：连续 reboot 10 次（guest `reboot -f`，
  terra-vmm 退出后立刻再拉起同一配置），每次都断言到 shell/login marker；
  记录每次耗时，波动异常要报

### M1.5 收尾验收

原五条验收标准维持不变，全部以真实 smoke 断言为准（不允许「没识别到也算过」
式的退路分支）；N1 的修复必须同时删除裸 `requested_size_arc` 直写路径，防止再犯。

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

M1 动态资源（见 6.1 节）→ **M1.5 VM 完备化 + Ubuntu bring-up（见 6.4 节，当前）** → M2 沙箱层 sandboxd、eBPF 观测、SDK/CLI/MCP（见 6.3 节，冻结中）→ M3 三级快照 → M4 sched_ext 与密度。**M2 之后评估 E2B 兼容适配层**（2026-07 与项目所有者定案要做：薄协议翻译层，不动自有内核 API 与 SDK/MCP 主接口，接存量 E2B 用户）。vmm-core 的设备管理抽象、VM 配置结构（`max_vcpu_count`、内存上限等字段）应能为后续里程碑直接扩展。

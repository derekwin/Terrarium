# 0001: 设备模型只用 virtio-mmio，不引入 PCI / ACPI

- 状态：已接受（M0，2026-07）
- 决策者：项目所有者

## 背景

microVM 需要一套设备发现与配置机制。传统 PC 体系用 PCI 枚举设备、
ACPI 表描述硬件拓扑与电源管理；QEMU/Firecracker 之外的极简 VMM
（如 kvmtool 的 mmio 模式、rust-vmm 参考实现）则把 virtio 设备直接
放在 MMIO 地址空间，用内核命令行（`virtio_mmio.device=`）声明。

## 决定

Terrarium 的设备模型**只用 virtio-mmio，永远不引入 PCI 与 ACPI**。
设备通过内核命令行 `virtio_mmio.device=<size>@<addr>:<irq>` 向 guest
声明（对应内核配置 `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`）。

## 理由

1. **复杂度**：PCI 需要实现配置空间、BAR、中断路由（INTx/MSI）、
   PCI 桥；ACPI 需要生成 DSDT 等 AML 表。两者合计是 Firecracker 级别
   VMM 中最大的设备子系统之一，而我们的设备屈指可数
   （blk/mem/balloon/vsock/console）。
2. **启动速度**：省去 PCI 枚举与 ACPI 表解析，内核走
   `virtio-mmio` 的平台设备路径直接 probe，符合冷启动 < 200ms 的目标。
3. **与资源模型匹配**：「启动预创建 + 运行调整」模型下，设备集合在
   启动时静态确定（磁盘容量等运行期变化走 virtio config change，
   与总线无关），不需要 PCI 热插拔语义。
4. **生态先例**：kvmtool、Cloud Hypervisor 的 `--console off --serial`
   极简配置、crosvm 均支持纯 virtio-mmio 形态；上游内核
   `virtio-mmio` 驱动维护良好。

## 代价与边界

- guest 内核必须启用 `CONFIG_VIRTIO_MMIO` 与
  `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`（xtask 的内核配置基线已包含）。
- 没有 ACPI 意味着没有 ACPI 电源键关机与 MADT CPU 枚举：关机走
  virtio 设备或 `reboot=k panic=-1` 命令行语义；CPU 枚举在 M0 单 vCPU
  下不需要 MP 表（内核自动按 UP 启动），M1 多 vCPU 时补 MP table
  （`mptable`，同样不需要 ACPI）。
- 该决定不可逆：任何引入 PCI / ACPI 的提议直接拒绝（AGENTS.md 第 1 节
  三个不可违背的架构决策之一）。

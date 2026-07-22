# 0003: virtio-mmio 地址/IRQ 布局与中断模型

- 状态：已接受（M1 Task 0，2026-07）
- 决策者：项目所有者

## 背景

ADR 0001 定了「只用 virtio-mmio、不引入 PCI/ACPI」的总原则，但没有定
具体布局：设备寄存器窗放在 guest 物理地址空间的哪里、每个设备占多大、
IRQ 怎么分配、guest 如何感知设备、中断如何注入。M1 Task 0 实现设备
框架前需要把这些定死，因为它们是 VMM 与 guest 内核之间的硬契约。

## 决定

### MMIO 地址布局

- 设备窗口基址 `0xd000_0000`（3.25GiB），每设备寄存器窗 4KiB
  （`0x1000`）、步长 4KiB，设备数上限 32（总占 128KiB）。
- 窗口位于 3GiB 低端内存顶之上、4GiB 之下的 MMIO 区，与将来的
  virtio-mem 热插拔区（M1 Task 3，规划在 4GiB 之上）互不冲突；
  也不挤占低端内存（e820 可用 RAM 不跨越 3GiB hole）。
- 4KiB 每设备是 virtio-mmio spec 建议值（寄存器区 0x100 字节 +
  配置空间在 0x100+），与 kvmtool / Firecracker 一致，内核
  `virtio_mmio.device=` 声明里的 size 写作 `4K`。

### IRQ 分配

- IRQ（GSI）从 5 起按注册顺序顺排（第 n 个设备 = IRQ 5+n）。
- 0~4 留给 in-kernel irqchip 的 legacy 设备：PIT 定时器 IRQ0、
  串口 COM1 IRQ4 等，避开冲突。

### guest 设备声明：内核 cmdline 而非 ACPI

设备经内核命令行向 guest 声明：

```
virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0001000:6 …
```

（对应 `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`，xtask 内核配置基线已包含。）
这是 ADR 0001「不引入 ACPI」决定的直接推论：没有 ACPI 就没有 DSDT
设备节点，cmdline 声明是 virtio-mmio 官方支持的无固件发现机制。
「启动预创建 + 运行调整」模型下设备集合在启动时静态确定，不需要
运行期发现/热插拔语义；运行期的容量变化走 virtio config change，
与总线发现机制无关。

### 中断模型

- 每个设备一条 GSI，电平触发，经 `KVM_IRQ_LINE` 注入 in-kernel
  irqchip（不引入 irqfd/eventfd，M1 设备少、vCPU 线程内同步处理，
  注入开销可忽略；后续若成瓶颈再议）。
- 中断状态寄存器（ISR）：bit0（used buffer）由设备的 `queue_notify`
  返回位置位；bit1（config change）等设备自发中断位经
  `pending_interrupts()` 在读出与判电平时并入。
- guest 写 InterruptACK 清传输层持有的位；设备自发位由设备自己清除
  （如驱动读完 config 后），ACK 清不掉。
- IRQ 电平 = 合并后的 ISR 非零；VMM 在每次 MMIO 访问后重算各设备电平，
  仅在电平变化时下发 `KVM_IRQ_LINE`（与串口 IRQ4 同款模式）。

### 队列实现：rust-vmm virtio-queue，不自写

virtqueue 描述符链解析（间接描述符、event_idx、环绕索引、对齐与
越界校验）是 virtio 设备最易写错的部分，rust-vmm 官方
`virtio-queue` crate 已被 Firecracker / Cloud Hypervisor / Dragonball
长期使用。传输层直接持有 `virtio_queue::Queue`：`set_ready` /
`set_*_address` 由寄存器写驱动，`is_valid(guest_mem)` 校验通过才允许
QueueReady 读回 1（非法队列配置按 spec 视为驱动失败）。描述符链遍历
（`iter`/`pop_descriptor_chain`/`add_used`/`needs_notification`）留给
Task 1 起的具体设备使用。

## 代价与边界

- cmdline 有长度上限（`CMDLINE_MAX_SIZE` 64KiB，32 个设备声明约
  1KiB，远未触及）。
- 设备数量、地址、IRQ 在 VM 启动后不可变；新增设备 = 重启 VM，
  与「启动预创建」模型一致。
- `virtio-queue` 的错误日志走 `log` crate；vmm-core 用 `tracing`，
  未接桥接器时这些日志不可见（只影响诊断详细度，不影响正确性）。
- 本 ADR 只覆盖传输层；具体设备（blk/mem/vsock）的寄存器语义在各自
  Task 的 ADR / 代码注释中定义。

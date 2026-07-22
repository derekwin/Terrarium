# 0002: 用 Linux x86 64-bit boot protocol 直接加载 bzImage

- 状态：已接受（M0，2026-07；2026-07 修订：入口策略从 64-bit 入口改为 32-bit 入口）

## 背景

引导 guest 内核的可选路径：

1. 固件（UEFI/OVMF）→ 完整 boot 链：违背极简原则，永远不做。
2. PVH（Xen）入口：需要 ELF 内核与额外 start_info 结构，生态较窄。
3. **Linux x86 64-bit boot protocol 直接加载 bzImage**：VMM 扮演
   bootloader，把 bzImage 的受保护模式内核读入 guest 内存，准备好
   zero page（boot_params）与 vCPU 寄存器后直接跳转。
   Firecracker / Dragonball / Cloud Hypervisor 的共同选择。

## 决定

采用方案 3，分工如下：

- **linux-loader（rust-vmm）**：解析 bzImage 头（`setup_header`），把
  受保护模式内核读到 0x100000（`HIMEM_START`），把内核命令行写到
  0x20000，并通过 `LinuxBootConfigurator` 把 boot_params 写到
  zero page（0x7000）。
- **vmm-core（arch/x86_64）**：负责 linux-loader 不管的部分——
  - 初始化 vCPU 进 **32-bit 保护模式**（`cr0 = PE|NE|ET`，不开分页、
    不开长模式），GDT/IDT 按 boot protocol 布局（0x500/0x520，
    扁平 CODE/DATA/TSS），`rsi` 指向 zero page，`rsp/rbp` 指向启动栈
    （0x8ff0），FPU 与 MSR 按常规上电值；
  - 填 boot_params 的业务字段：e820 内存表、命令行指针与长度、
    initrd 地址与大小、`type_of_loader=0xff`。
- **入口地址**：32-bit 入口 `startup_32`，即受保护模式内核镜像基址
  0x100000（`rip = 内核加载地址`，无偏移）。内核解压器
  （`arch/x86/boot/compressed/head_64.S`）自己完成页表搭建与长模式
  切换。这与 QEMU `-kernel`（linuxboot）的入口方式一致。

### 为什么从 64-bit 入口改为 32-bit 入口

初版按 Firecracker/Dragonball 的做法实现 64-bit 入口（VMM 搭恒等映射
页表、开长模式、跳到 `startup_64` = 镜像基址 + 0x200）。改为 32-bit
入口的理由：

- VMM 不再需要搭建/维护启动页表，启动路径少一整块状态（页表地址
  分配、2MiB 大页项、CR3/CR4/EFER 初始化）；
- 入口契约更简单：保护模式 + 扁平段 + `rsi`=zero page，其余全由
  内核解压器自理；这也是 QEMU 验证过十几年的路径；
- 64-bit 入口省下的只是解压器里几条指令，对冷启动耗时无感。

## 关键实现事实（排障记录）

- **VMCS 的 segment limit 必须按 G 位缩放后再写入**（M0 最大的坑）。
  KVM_SET_SREGS 把 `kvm_segment.limit` 原样写进 VMCS，KVM 不做 G 位
  缩放。Dragonball/Firecracker 的 `gdt.rs` 直接搬原始 20-bit limit
  （0xfffff, G=1）也能跑，是因为它们从 64-bit 入口进长模式，长模式下
  段限检查关闭；从 32-bit 入口进保护模式时，未缩放的 limit 使实际段限
  卡在 1MiB，≥0x100000 取指立即 #GP，经空 IDT 变三重故障
  （KVM_EXIT_SHUTDOWN，rip 不变，无任何串口输出）。
  本仓库 `kvm_segment_from_gdt` 在构造时完成缩放（G=1 时
  `limit = (raw << 12) | 0xfff`）。
- **irqchip 之外还需要 in-kernel PIT**：`KVM_CREATE_IRQCHIP` 不含 i8253
  PIT；缺了 `KVM_CREATE_PIT2`，guest 对 0x40/0x43 的访问全部退到
  用户态，内核早期 TSC 校准时钟读数恒为 0xff，死循环卡死在 console
  初始化之前（本仓库 M0 调试实测）。两者都必须在创建 vCPU 之前建好。
  `KVM_PIT_SPEAKER_DUMMY` 同时让 0x61 端口在内核态处理，否则解压器
  KASLR 的 i8254 熵源会死等 channel 2 计数。
- **LAPIC LINT 必须显式配置**：LAPIC 复位后 LVT LINT0/LINT1 处于 masked
  状态，需要把 LINT0 设为 ExtINT（接 8259A PIC 输出）、LINT1 设为 NMI，
  否则 PIC 送来的定时器中断永远到不了 CPU（来源：dbs_arch
  `interrupts.rs` 的 `set_lint`）。
- **为什么不用 ELF 加载器**：Firecracker/Dragonball 加载的是 ELF
  （vmlinux），其 `kernel_load` 即 ELF 入口点；xtask 的产物是 bzImage，
  用 `bzimage` 加载器时 `kernel_load` 是镜像基址（= startup_32）。
- **MP table 暂缓**：单 vCPU 且无 ACPI 时内核自动按 UP 启动，不需要
  MP 表；M1 引入多 vCPU 时再补（mptable 放 640KiB 基本内存末尾 1KiB，
  与 ACPI 无关）。

## 参考

- 内核 `Documentation/arch/x86/boot.rst`（64-bit boot protocol，含
  32-bit 入口约定）
- linux-loader 0.14 `loader/bzimage`、`configurator/x86_64/linux`
- Dragonball `dbs_boot` / `dbs_arch`（GDT、寄存器初始化常量来源；
  注意其段限未缩放问题见上）

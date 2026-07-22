// Copyright 2021-2022 Alibaba Cloud. All Rights Reserved.
// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
//
// 本文件移植自 Dragonball（kata-containers 仓库，commit 809ab7d90f7dc8c10f51e5b0eef55b9bd33cdbc5）：
//   - src/dragonball/crates/dbs_boot/src/x86_64/layout.rs   —— guest 内存布局常量
//   - src/dragonball/crates/dbs_boot/src/x86_64/mod.rs      —— 恒等映射页表、e820、initrd 加载地址
//   - src/dragonball/crates/dbs_arch/src/x86_64/regs.rs     —— vCPU regs/sregs/fpu/msr 初始化
//   - src/dragonball/crates/dbs_arch/src/x86_64/gdt.rs      —— GDT 表项构造与解析
//   - src/dragonball/crates/dbs_arch/src/x86_64/msr.rs      —— MSR 索引常量（片段）
//   - src/dragonball/crates/dbs_arch/src/x86_64/interrupts.rs —— LAPIC LINT 配置
// 移植后按本项目规范改造：中文注释、去掉本项目用不到的部分（MP table、mpspec 等）。
// 已登记到 THIRD-PARTY。

//! x86_64 平台相关代码：guest 物理内存布局、启动页表、GDT/IDT、vCPU 寄存器初始化、
//! boot params（zero page）构造。
//!
//! 参考：Linux x86 boot protocol（内核源码 `Documentation/arch/x86/boot.rst`）。

use kvm_bindings::{kvm_fpu, kvm_msr_entry, kvm_regs, kvm_segment, kvm_sregs, Msrs};
use kvm_ioctls::VcpuFd;
use linux_loader::loader::bootparam;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};

// ---------------------------------------------------------------------------
// guest 物理内存布局常量（移植自 dbs_boot/src/x86_64/layout.rs）
// ---------------------------------------------------------------------------

/// GDT 在 guest 内存中的地址。
pub const BOOT_GDT_OFFSET: u64 = 0x500;
/// IDT 在 guest 内存中的地址。
pub const BOOT_IDT_OFFSET: u64 = 0x520;
/// 初始 GDT 表项数（NULL / CODE / DATA / TSS）。
pub const BOOT_GDT_MAX: usize = 4;
/// zero page（boot_params）地址。
pub const ZERO_PAGE_START: u64 = 0x7000;
/// 启动 vCPU 的初始栈指针。
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
/// 内核命令行在 guest 内存中的起始地址。
pub const CMDLINE_START: u64 = 0x20000;
/// 内核命令行最大长度（含 NUL）。
pub const CMDLINE_MAX_SIZE: usize = 0x10000;
/// 高端内存起始地址（1MiB），内核镜像加载到这里；32-bit 入口
/// startup_32 也在该地址（见 ADR 0002 的入口策略决定）。
pub const HIMEM_START: u64 = 0x0010_0000;
/// EBDA 起始地址；[0, EBDA_START) 是 e820 中的第一段可用 RAM。
pub const EBDA_START: u64 = 0x9fc00;

/// MP table 起始地址（EBDA_START - 1KiB = 640KiB 基本内存末尾）。
pub const MPTABLE_START: u64 = 0x9f800;

/// e820 表项类型：可用 RAM（内核 uapi `asm/e820.h`）。
const E820_RAM: u32 = 1;

/// guest 页大小（4KiB）。
pub const PAGE_SIZE: u64 = 4096;

// ---------------------------------------------------------------------------
// 控制寄存器位（移植自 dbs_arch/src/x86_64/regs.rs）
// ---------------------------------------------------------------------------

/// CR0：保护模式使能。
pub const X86_CR0_PE: u64 = 0x1;
/// CR0：数字错误（VMX fixed-1 位）。
pub const X86_CR0_NE: u64 = 0x20;
/// CR0：协处理器扩展类型（486+ 恒为 1）。
pub const X86_CR0_ET: u64 = 0x10;

// MTRR 默认类型相关位（IA32_MTRR_DEF_TYPE）。
const MTRR_ENABLED: u64 = 0x0800;
const MTRR_FIXED_RANGE_ENABLE: u64 = 0x0400;
const MTRR_MEM_TYPE_WB: u64 = 0x6;

// ---------------------------------------------------------------------------
// MSR 索引常量（移植自 dbs_arch/src/x86_64/msr.rs，只保留本项目用到的）
// ---------------------------------------------------------------------------

const MSR_IA32_SYSENTER_CS: u32 = 0x174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x176;
const MSR_MTRR_DEF_TYPE: u32 = 0x2ff;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;
const MSR_SYSCALL_MASK: u32 = 0xc000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;
const MSR_IA32_TSC: u32 = 0x10;
const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
const MSR_IA32_MISC_ENABLE_FAST_STRING: u64 = 0x1;

// ---------------------------------------------------------------------------
// LAPIC LINT 配置（移植自 dbs_arch/src/x86_64/interrupts.rs）
// ---------------------------------------------------------------------------

// 常量取自内核 apicdef.h。
const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_MODE_NMI: u32 = 0x4;
const APIC_MODE_EXTINT: u32 = 0x7;

fn get_klapic_reg(klapic: &kvm_bindings::kvm_lapic_state, reg_offset: usize) -> u32 {
    let range = reg_offset..reg_offset + 4;
    let reg = klapic.regs.get(range).expect("get_klapic_reg range");

    let mut reg_bytes = [0u8; 4];
    for (byte, read) in reg_bytes.iter_mut().zip(reg.iter().cloned()) {
        *byte = read as u8;
    }
    u32::from_le_bytes(reg_bytes)
}

fn set_klapic_reg(klapic: &mut kvm_bindings::kvm_lapic_state, reg_offset: usize, value: u32) {
    let range = reg_offset..reg_offset + 4;
    let reg = klapic.regs.get_mut(range).expect("set_klapic_reg range");

    let value = u32::to_le_bytes(value);
    for (byte, read) in reg.iter_mut().zip(value.iter().cloned()) {
        *byte = read as i8;
    }
}

fn set_apic_delivery_mode(reg: u32, mode: u32) -> u32 {
    (reg & !0x700) | (mode << 8)
}

/// 配置 LAPIC LINT：LINT0 设为 ExtINT（接 8259A PIC 的定时器等中断），
/// LINT1 设为 NMI。
///
/// 缺了这一步，LAPIC 复位值里 LVT 项是 masked，PIC 的中断永远送不到 CPU，
/// guest 早期定时器校准（timer_irq_works）会无退出地死等。
pub fn set_lint(vcpu: &VcpuFd) -> Result<()> {
    let mut klapic = vcpu.get_lapic().map_err(Error::GetLapic)?;

    let lvt_lint0 = get_klapic_reg(&klapic, APIC_LVT0);
    set_klapic_reg(
        &mut klapic,
        APIC_LVT0,
        set_apic_delivery_mode(lvt_lint0, APIC_MODE_EXTINT),
    );
    let lvt_lint1 = get_klapic_reg(&klapic, APIC_LVT1);
    set_klapic_reg(
        &mut klapic,
        APIC_LVT1,
        set_apic_delivery_mode(lvt_lint1, APIC_MODE_NMI),
    );

    vcpu.set_lapic(&klapic).map_err(Error::SetLapic)
}

// ---------------------------------------------------------------------------
// 错误类型
// ---------------------------------------------------------------------------

/// x86_64 平台初始化错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 写 GDT 到 guest 内存失败。
    #[error("写 GDT 到 guest 内存失败")]
    WriteGdt,
    /// 写 IDT 到 guest 内存失败。
    #[error("写 IDT 到 guest 内存失败")]
    WriteIdt,
    /// e820 表已满，无法继续添加表项。
    #[error("e820 表已满，无法继续添加表项")]
    E820Full,
    /// initrd 放不下（低端内存不足）。
    #[error("guest 低端内存不足以容纳 initrd")]
    InitrdAddress,
    /// 读 vCPU sregs 失败。
    #[error("读 vCPU sregs 失败: {0}")]
    GetSregs(kvm_ioctls::Error),
    /// 写 vCPU sregs 失败。
    #[error("写 vCPU sregs 失败: {0}")]
    SetSregs(kvm_ioctls::Error),
    /// 写 vCPU 通用寄存器失败。
    #[error("写 vCPU 通用寄存器失败: {0}")]
    SetRegs(kvm_ioctls::Error),
    /// 写 vCPU FPU 寄存器失败。
    #[error("写 vCPU FPU 寄存器失败: {0}")]
    SetFpu(kvm_ioctls::Error),
    /// 写 vCPU MSR 失败。
    #[error("写 vCPU MSR 失败: {0}")]
    SetMsrs(kvm_ioctls::Error),
    /// KVM_SET_MSRS 写入的 MSR 数量与预期不符。
    #[error("KVM_SET_MSRS 写入的 MSR 数量与预期不符")]
    SetMsrsCount,
    /// 读 LAPIC 状态失败。
    #[error("读 LAPIC 状态失败: {0}")]
    GetLapic(kvm_ioctls::Error),
    /// 写 LAPIC 状态失败。
    #[error("写 LAPIC 状态失败: {0}")]
    SetLapic(kvm_ioctls::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// GDT（移植自 dbs_arch/src/x86_64/gdt.rs）
// ---------------------------------------------------------------------------

/// 按内核 `arch/x86/include/asm/segment.h` 的位布局构造一个 GDT 表项。
pub fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000) << (56 - 24))
        | ((u64::from(flags) & 0x0000_f0ff) << 40)
        | ((u64::from(limit) & 0x000f_0000) << (48 - 16))
        | ((u64::from(base) & 0x00ff_ffff) << 16)
        | (u64::from(limit) & 0x0000_ffff)
}

fn get_base(entry: u64) -> u64 {
    ((entry & 0xff00_0000_0000_0000) >> 32)
        | ((entry & 0x0000_00ff_0000_0000) >> 16)
        | ((entry & 0x0000_0000_ffff_0000) >> 16)
}

fn get_limit(entry: u64) -> u32 {
    (((entry & 0x000f_0000_0000_0000) >> 32) | (entry & 0x0000_0000_0000_ffff)) as u32
}

fn get_g(entry: u64) -> u8 {
    ((entry & 0x0080_0000_0000_0000) >> 55) as u8
}

fn get_db(entry: u64) -> u8 {
    ((entry & 0x0040_0000_0000_0000) >> 54) as u8
}

fn get_l(entry: u64) -> u8 {
    ((entry & 0x0020_0000_0000_0000) >> 53) as u8
}

fn get_avl(entry: u64) -> u8 {
    ((entry & 0x0010_0000_0000_0000) >> 52) as u8
}

fn get_p(entry: u64) -> u8 {
    ((entry & 0x0000_8000_0000_0000) >> 47) as u8
}

fn get_dpl(entry: u64) -> u8 {
    ((entry & 0x0000_6000_0000_0000) >> 45) as u8
}

fn get_s(entry: u64) -> u8 {
    ((entry & 0x0000_1000_0000_0000) >> 44) as u8
}

fn get_type(entry: u64) -> u8 {
    ((entry & 0x0000_0f00_0000_0000) >> 40) as u8
}

/// 由 GDT 表项构造 KVM_SET_SREGS 需要的 `kvm_segment`。
///
/// 注意：VMCS 的 segment limit 字段存放的是**按 G 位缩放后**的值（KVM 不会
/// 代劳）。Dragonball/Firecracker 从 64-bit 入口进长模式，段限被忽略，原始值
/// 不缩放也能跑；我们从 32-bit 入口进保护模式，段限真实生效——不缩放的话
/// 实际段限只有 1MiB，≥0x100000 取指立即 #GP（M0 调试实测）。
pub fn kvm_segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    let g = get_g(entry);
    let raw_limit = get_limit(entry);
    let limit = if g != 0 {
        (raw_limit << 12) | 0xfff
    } else {
        raw_limit
    };
    kvm_segment {
        base: get_base(entry),
        limit,
        selector: u16::from(table_index * 8),
        type_: get_type(entry),
        present: get_p(entry),
        dpl: get_dpl(entry),
        db: get_db(entry),
        s: get_s(entry),
        l: get_l(entry),
        g,
        avl: get_avl(entry),
        padding: 0,
        unusable: u8::from(get_p(entry) == 0),
    }
}

/// 32-bit 入口要求的初始 GDT：NULL / 扁平 CODE / 扁平 DATA / TSS。
/// （与 Cloud Hypervisor 的保护模式入口配置一致。）
pub fn boot_gdt_table() -> [u64; BOOT_GDT_MAX] {
    [
        gdt_entry(0, 0, 0),                // NULL
        gdt_entry(0xc09b, 0, 0xffff_ffff), // CODE：32-bit 扁平，base=0 limit=4GiB
        gdt_entry(0xc093, 0, 0xffff_ffff), // DATA：32-bit 扁平
        gdt_entry(0x008b, 0, 0x67),        // TSS
    ]
}

// ---------------------------------------------------------------------------
// vCPU 寄存器初始化（移植自 dbs_arch/src/x86_64/regs.rs）
// ---------------------------------------------------------------------------

/// 初始化 FPU 寄存器。
pub fn setup_fpu(vcpu: &VcpuFd) -> Result<()> {
    let fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu).map_err(Error::SetFpu)
}

/// 初始化 MSR。
pub fn setup_msrs(vcpu: &VcpuFd) -> Result<()> {
    let entry_vec = create_msr_entries();
    let kvm_msrs = Msrs::from_entries(&entry_vec).map_err(|_| Error::SetMsrsCount)?;

    vcpu.set_msrs(&kvm_msrs)
        .map_err(Error::SetMsrs)
        .and_then(|msrs_written| {
            if msrs_written as u32 != kvm_msrs.as_fam_struct_ref().nmsrs {
                Err(Error::SetMsrsCount)
            } else {
                Ok(msrs_written)
            }
        })?;
    Ok(())
}

/// 初始化通用寄存器：`rip` 指向内核 64-bit 入口，`rsi` 指向 zero page（boot protocol 要求）。
pub fn setup_regs(vcpu: &VcpuFd, boot_ip: u64) -> Result<()> {
    let regs = kvm_regs {
        rflags: 0x0000_0000_0000_0002u64,
        rip: boot_ip,
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rsi: ZERO_PAGE_START,
        ..Default::default()
    };
    vcpu.set_regs(&regs).map_err(Error::SetRegs)
}

/// 初始化段寄存器与控制寄存器：写入 GDT/IDT，进入 32-bit 保护模式
/// （不开分页、不开长模式；内核解压器自行完成向长模式的切换，见 ADR 0002）。
pub fn setup_sregs<M: GuestMemoryBackend>(mem: &M, vcpu: &VcpuFd) -> Result<()> {
    let gdt_table = boot_gdt_table();
    let mut sregs: kvm_sregs = vcpu.get_sregs().map_err(Error::GetSregs)?;
    configure_segments_and_sregs(mem, &mut sregs, &gdt_table)?;
    vcpu.set_sregs(&sregs).map_err(Error::SetSregs)
}

fn configure_segments_and_sregs<M: GuestMemoryBackend>(
    mem: &M,
    sregs: &mut kvm_sregs,
    gdt_table: &[u64; BOOT_GDT_MAX],
) -> Result<()> {
    let code_seg = kvm_segment_from_gdt(gdt_table[1], 1);
    let data_seg = kvm_segment_from_gdt(gdt_table[2], 2);
    let tss_seg = kvm_segment_from_gdt(gdt_table[3], 3);

    // 写 GDT 到 guest 内存。
    let gdt_addr = GuestAddress(BOOT_GDT_OFFSET);
    for (index, entry) in gdt_table.iter().enumerate() {
        let addr = mem
            .checked_offset(gdt_addr, index * std::mem::size_of::<u64>())
            .ok_or(Error::WriteGdt)?;
        mem.write_obj(*entry, addr).map_err(|_| Error::WriteGdt)?;
    }
    sregs.gdt.base = BOOT_GDT_OFFSET;
    sregs.gdt.limit = std::mem::size_of_val(gdt_table) as u16 - 1;

    // 写空 IDT。
    mem.write_obj(0u64, GuestAddress(BOOT_IDT_OFFSET))
        .map_err(|_| Error::WriteIdt)?;
    sregs.idt.base = BOOT_IDT_OFFSET;
    sregs.idt.limit = std::mem::size_of::<u64>() as u16 - 1;

    sregs.cs = code_seg;
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;
    sregs.tr = tss_seg;

    // 32-bit 保护模式：cr0 设 PE；NE/ET 是 VMX 的 fixed-1 位（缺了会 vmentry 失败），
    // 分页与长模式由内核解压器自己开启。
    sregs.cr0 = X86_CR0_PE | X86_CR0_NE | X86_CR0_ET;
    sregs.cr4 = 0;

    Ok(())
}

#[allow(clippy::vec_init_then_push)]
fn create_msr_entries() -> Vec<kvm_msr_entry> {
    let mut entries = Vec::<kvm_msr_entry>::new();

    entries.push(kvm_msr_entry {
        index: MSR_IA32_SYSENTER_CS,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_IA32_SYSENTER_ESP,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_IA32_SYSENTER_EIP,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_MTRR_DEF_TYPE,
        data: MTRR_ENABLED | MTRR_FIXED_RANGE_ENABLE | MTRR_MEM_TYPE_WB,
        ..Default::default()
    });
    // 以下为 x86_64 专有 MSR。
    entries.push(kvm_msr_entry {
        index: MSR_STAR,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_CSTAR,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_KERNEL_GS_BASE,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_SYSCALL_MASK,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_LSTAR,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_IA32_TSC,
        data: 0x0,
        ..Default::default()
    });
    entries.push(kvm_msr_entry {
        index: MSR_IA32_MISC_ENABLE,
        data: MSR_IA32_MISC_ENABLE_FAST_STRING,
        ..Default::default()
    });

    entries
}

// ---------------------------------------------------------------------------
// boot params / e820 / initrd（移植自 dbs_boot 与 dragonball src/vm/x86_64.rs）
// ---------------------------------------------------------------------------

/// 计算 initrd 的加载地址：放在低端内存顶部、按页对齐。
///
/// 为避免与内核镜像和 boot params 重叠，低端预留 32MiB。
pub fn initrd_load_addr<M: GuestMemoryBackend>(guest_mem: &M, initrd_size: u64) -> Result<u64> {
    let lowmem_size = guest_mem
        .find_region(GuestAddress(0))
        .ok_or(Error::InitrdAddress)?
        .len();

    if lowmem_size < initrd_size + (32 << 20) {
        return Err(Error::InitrdAddress);
    }

    Ok((lowmem_size - initrd_size) & !(PAGE_SIZE - 1))
}

/// 向 e820 表追加一个表项；表满时报错。
pub fn add_e820_entry(
    params: &mut bootparam::boot_params,
    addr: u64,
    size: u64,
    mem_type: u32,
) -> Result<()> {
    if params.e820_entries >= params.e820_table.len() as u8 {
        return Err(Error::E820Full);
    }

    params.e820_table[params.e820_entries as usize].addr = addr;
    params.e820_table[params.e820_entries as usize].size = size;
    params.e820_table[params.e820_entries as usize].r#type = mem_type;
    params.e820_entries += 1;

    Ok(())
}

/// 构造 boot_params（zero page）。
///
/// - `setup_header`：由 linux-loader 从 bzImage 头读出的原始 setup header；
/// - `mem_size`：guest 内存总字节数（本项目 M0 只支持单段内存、不跨越 3GiB MMIO hole）；
/// - `cmdline_size`：命令行字节数（含 NUL）；
/// - `initrd`：`Some((加载地址, 字节数))`。
/// - `boot_cpus`：启动时激活的 vCPU 数（用于 MP table）。
pub fn build_boot_params(
    setup_header: bootparam::setup_header,
    mem_size: u64,
    cmdline_size: u32,
    initrd: Option<(u32, u32)>,
    boot_cpus: u8,
) -> Result<bootparam::boot_params> {
    const KERNEL_LOADER_OTHER: u8 = 0xff;

    let mut params = bootparam::boot_params {
        hdr: setup_header,
        ..Default::default()
    };
    params.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    params.hdr.cmd_line_ptr = CMDLINE_START as u32;
    params.hdr.cmdline_size = cmdline_size;
    if let Some((addr, size)) = initrd {
        params.hdr.ramdisk_image = addr;
        params.hdr.ramdisk_size = size;
    }

    // e820：[0, EBDA_START) 可用；[EBDA_START, 1MiB) 保留（不写表项即默认保留）；
    // [1MiB, mem_size) 可用。
    add_e820_entry(&mut params, 0, EBDA_START, E820_RAM)?;
    add_e820_entry(&mut params, HIMEM_START, mem_size - HIMEM_START, E820_RAM)?;

    Ok(params)
}

/// 在 guest 内存中写入 MP table（多 vCPU 枚举）。
///
/// 内核通过 MP Floating Pointer Structure 发现 MP Configuration Table，
/// 从中读取 CPU 数量和 LAPIC ID。放在 640KiB 基本内存末尾（`MPTABLE_START`）。
pub fn setup_mp_table(mem: &impl Bytes<GuestAddress>, num_cpus: u8) -> Result<()> {
    use vm_memory::Address;

    let base = GuestAddress(MPTABLE_START);
    let ioapic_id = num_cpus + 1;

    // 辅助函数：写 u8/u16/u32/u64 小端字节
    let write_u8 = |addr: GuestAddress, v: u8| -> Result<()> {
        mem.write_slice(&[v], addr).map_err(|_| Error::WriteGdt)
    };
    let write_u16 = |addr: GuestAddress, v: u16| -> Result<()> {
        mem.write_slice(&v.to_le_bytes(), addr)
            .map_err(|_| Error::WriteGdt)
    };
    let write_u32 = |addr: GuestAddress, v: u32| -> Result<()> {
        mem.write_slice(&v.to_le_bytes(), addr)
            .map_err(|_| Error::WriteGdt)
    };
    let _write_u64 = |addr: GuestAddress, v: u64| -> Result<()> {
        mem.write_slice(&v.to_le_bytes(), addr)
            .map_err(|_| Error::WriteGdt)
    };
    let write_slice = |addr: GuestAddress, data: &[u8]| -> Result<()> {
        mem.write_slice(data, addr).map_err(|_| Error::WriteGdt)
    };

    // 1. MP Floating Pointer Structure (16 bytes)
    let mut pos = base;
    // signature: "_MP_"
    write_slice(pos, b"_MP_")?;
    // physptr: points to MP config table (next 16 bytes)
    write_u32(pos.unchecked_add(4), (pos.0 + 16) as u32)?;
    // length: 1 paragraph (16 bytes)
    write_u8(pos.unchecked_add(8), 1)?;
    // spec: version 4
    write_u8(pos.unchecked_add(9), 4)?;
    // checksum placeholder (will fixup later)
    write_u8(pos.unchecked_add(10), 0)?;
    // feature bytes: 0
    write_u8(pos.unchecked_add(11), 0)?;
    write_u8(pos.unchecked_add(12), 0)?;
    write_u8(pos.unchecked_add(13), 0)?;
    write_u8(pos.unchecked_add(14), 0)?;
    write_u8(pos.unchecked_add(15), 0)?;

    // Fix MP floating pointer checksum
    let mut mpf_bytes = vec![0u8; 16];
    mem.read_slice(&mut mpf_bytes, base)
        .map_err(|_| Error::WriteGdt)?;
    let sum: u8 = mpf_bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b));
    write_u8(pos.unchecked_add(10), (!sum).wrapping_add(1))?;

    // 2. MP Configuration Table
    // Skip header placeholder (44 bytes), fill later
    let table_base = pos.unchecked_add(16);
    let header_pos = table_base;
    pos = table_base.unchecked_add(44);
    let mut checksum: u8 = 0;

    // Helper: record checksum for written bytes
    let mut write_and_sum = |addr: GuestAddress, data: &[u8]| -> Result<()> {
        write_slice(addr, data)?;
        for b in data {
            checksum = checksum.wrapping_add(*b);
        }
        Ok(())
    };

    // CPU entries (20 bytes each)
    for cpu_id in 0..num_cpus {
        let mut cpu_bytes = [0u8; 20];
        cpu_bytes[0] = 0; // entry_type = processor
        cpu_bytes[1] = cpu_id; // apic_id
        cpu_bytes[2] = 0x14; // apic_version
        cpu_bytes[3] = if cpu_id == 0 { 3 } else { 1 }; // flags: BSP+enabled or just enabled
                                                        // cpu_signature: stepping 0x600
        cpu_bytes[4..8].copy_from_slice(&0x600u32.to_le_bytes());
        // feature_flags: FPU(0x1) | APIC(0x200) = 0x201
        cpu_bytes[8..12].copy_from_slice(&0x201u32.to_le_bytes());
        // reserved[8]: already zero
        write_and_sum(pos, &cpu_bytes)?;
        pos = pos.unchecked_add(20);
    }

    // ISA bus entry (8 bytes)
    write_and_sum(pos, &[1, 0, b'I', b'S', b'A', b' ', b' ', b' '])?;
    pos = pos.unchecked_add(8);

    // PCI bus entry (8 bytes)
    write_and_sum(pos, &[1, 1, b'P', b'C', b'I', b' ', b' ', b' '])?;
    pos = pos.unchecked_add(8);

    // IOAPIC entry (8 bytes)
    let mut ioapic_bytes = [0u8; 8];
    ioapic_bytes[0] = 2; // entry_type = IOAPIC
    ioapic_bytes[1] = ioapic_id;
    ioapic_bytes[2] = 0x14; // apic_version
    ioapic_bytes[3] = 1; // flags: usable
    ioapic_bytes[4..8].copy_from_slice(&0xfec0_0000u32.to_le_bytes());
    write_and_sum(pos, &ioapic_bytes)?;
    pos = pos.unchecked_add(8);

    // Interrupt source entries (8 bytes each, IRQ 0-15, skip IRQ 2)
    for i in 0..16u8 {
        if i == 2 {
            continue;
        }
        let dst_irq = if i == 0 { 2u8 } else { i };
        let mut int_bytes = [0u8; 8];
        int_bytes[0] = 3; // entry_type = intsrc
        int_bytes[1] = 0; // irq_type = INT
                          // flags: u16le, 0 (conforms to spec)
        int_bytes[4] = 0; // src_bus = ISA
        int_bytes[5] = i; // src_irq
        int_bytes[6] = ioapic_id; // dst_apic
        int_bytes[7] = dst_irq;
        write_and_sum(pos, &int_bytes)?;
        pos = pos.unchecked_add(8);
    }

    // Local interrupt: ExtINT (8 bytes)
    write_and_sum(pos, &[4, 3, 0, 0, 0, 0, 0, 0])?;
    pos = pos.unchecked_add(8);

    // Local interrupt: NMI (8 bytes)
    write_and_sum(pos, &[4, 1, 0, 0, 0, 0, 0xff, 1])?;
    pos = pos.unchecked_add(8);

    // 3. Fill MP Configuration Table header (44 bytes)
    let table_length = pos.unchecked_offset_from(header_pos) as u16;
    // Write header fields
    write_slice(header_pos, b"PCMP")?;
    write_u16(header_pos.unchecked_add(4), table_length)?;
    write_u8(header_pos.unchecked_add(6), 4)?; // spec version
                                               // checksum placeholder
    write_u8(header_pos.unchecked_add(7), 0)?;
    // OEM: "TERRA   "
    write_slice(header_pos.unchecked_add(8), b"TERRA   ")?;
    // Product: "TERRA       "
    write_slice(header_pos.unchecked_add(16), b"TERRA       ")?;
    // oemptr, oemsize
    write_u32(header_pos.unchecked_add(28), 0)?;
    write_u16(header_pos.unchecked_add(32), 0)?;
    // entry_count
    write_u16(header_pos.unchecked_add(34), num_cpus as u16 + 2 + 15 + 2)?;
    // lapic address
    write_u32(header_pos.unchecked_add(36), 0xfee0_0000)?;
    // ext_length, ext_checksum, reserved
    write_u16(header_pos.unchecked_add(40), 0)?;
    write_u8(header_pos.unchecked_add(42), 0)?;
    write_u8(header_pos.unchecked_add(43), 0)?;

    // Fix header checksum
    let mut header_bytes = vec![0u8; 44];
    mem.read_slice(&mut header_bytes, header_pos)
        .map_err(|_| Error::WriteGdt)?;
    checksum = checksum.wrapping_add(header_bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    // Subtract placeholder checksum byte
    checksum = checksum.wrapping_sub(header_bytes[7]);
    write_u8(header_pos.unchecked_add(7), (!checksum).wrapping_add(1))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestMemoryMmap;

    fn create_guest_mem(size: usize) -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size)]).unwrap()
    }

    #[test]
    fn test_gdt_entry_roundtrip() {
        // 取自 dbs_arch gdt.rs 的测试：验证表项编码与解析互逆。
        // 注意：limit 断言的是按 G 位缩放后的值（见 kvm_segment_from_gdt 注释）。
        let gdt = gdt_entry(0xa09b, 0x10_0000, 0xfffff);
        let seg = kvm_segment_from_gdt(gdt, 0);
        assert_eq!(1, seg.g);
        assert_eq!(0, seg.db);
        assert_eq!(1, seg.l);
        assert_eq!(0, seg.avl);
        assert_eq!(1, seg.present);
        assert_eq!(0, seg.dpl);
        assert_eq!(1, seg.s);
        assert_eq!(0xb, seg.type_);
        assert_eq!(0x10_0000, seg.base);
        assert_eq!(0xffff_ffff, seg.limit);
        assert_eq!(0, seg.unusable);
    }

    #[test]
    fn test_boot_gdt_table_encoding() {
        // 32-bit 保护模式扁平段（与 Cloud Hypervisor 测试值一致）。
        let gdt = boot_gdt_table();
        assert_eq!(0x0, gdt[0]);
        assert_eq!(0xcf_9b00_0000_ffff, gdt[1]);
        assert_eq!(0xcf_9300_0000_ffff, gdt[2]);
        assert_eq!(0x8b00_0000_0067, gdt[3]);
    }

    #[test]
    fn test_initrd_load_addr() {
        // 128MiB 内存：initrd 顶部对齐放在内存末尾。
        let gm = create_guest_mem(128 << 20);
        let addr = initrd_load_addr(&gm, 4097).unwrap();
        assert_eq!((128 << 20) - 4097_u64.div_ceil(4096) * 4096, addr);
        assert_eq!(0, addr % PAGE_SIZE);

        // 内存太小（< initrd + 32MiB 预留）时报错。
        let gm = create_guest_mem(32 << 20);
        assert!(initrd_load_addr(&gm, 4096).is_err());
    }

    #[test]
    fn test_add_e820_entry() {
        let mut params = bootparam::boot_params::default();
        add_e820_entry(&mut params, 0x1000, 0x2000, E820_RAM).unwrap();
        assert_eq!(1, params.e820_entries);
        // boot_e820_entry 是 packed 结构，先按值拷贝到局部变量再断言。
        let (addr, size, mem_type) = (
            params.e820_table[0].addr,
            params.e820_table[0].size,
            params.e820_table[0].r#type,
        );
        assert_eq!((0x1000, 0x2000, E820_RAM), (addr, size, mem_type));

        // 表满时报错。
        params.e820_entries = params.e820_table.len() as u8;
        assert!(add_e820_entry(&mut params, 0, 0, E820_RAM).is_err());
    }

    #[test]
    fn test_build_boot_params() {
        let setup_header = bootparam::setup_header {
            header: 0x5372_6448,
            version: 0x020f,
            ..Default::default()
        };
        let mem_size: u64 = 128 << 20;
        let params =
            build_boot_params(setup_header, mem_size, 32, Some((0x7000_0000, 0x1000)), 1).unwrap();

        // setup_header 原样保留，type_of_loader 被覆写。
        // 注意：hdr/e820_table 是 packed 字段，按值拷贝后再断言。
        let hdr = params.hdr;
        assert_eq!(0x5372_6448, { hdr.header });
        assert_eq!(0xff, { hdr.type_of_loader });
        assert_eq!(CMDLINE_START as u32, { hdr.cmd_line_ptr });
        assert_eq!(32, { hdr.cmdline_size });
        assert_eq!(0x7000_0000, { hdr.ramdisk_image });
        assert_eq!(0x1000, { hdr.ramdisk_size });

        // e820 两段：[0, EBDA) 与 [1MiB, mem_size)。
        assert_eq!(2, params.e820_entries);
        let e0 = params.e820_table[0];
        let e1 = params.e820_table[1];
        assert_eq!((0, EBDA_START), ({ e0.addr }, { e0.size }));
        assert_eq!(
            (HIMEM_START, mem_size - HIMEM_START),
            ({ e1.addr }, { e1.size })
        );
    }
}

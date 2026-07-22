//! 最小 16550A UART 仿真（PIO，COM1 = 0x3f8）。
//!
//! 本项目自研的最小实现（没有采用 vm-superio，避免引入额外依赖；Dragonball 的
//! `dbs_legacy_devices/src/serial.rs` 正是基于 vm-superio 的）。
//!
//! M0 范围：
//! - 输出方向：guest 写 THR 的字节直接打到 host 输出（`Vm` 用 stdout），
//!   发送恒瞬时完成；IER.THRI 使能时经 IRQ4 上报 THR 空中断（Linux 8250
//!   tty 写路径依赖它，否则会卡在第一个 FIFO 之后）；
//! - 输入方向：保留 `enqueue_input` 注入接口与 RBR/LSR.DR 读路径，但 M0
//!   不接线、也不产生接收中断。
//!
//! 状态机只实现 Linux 8250 驱动探测与收发所需的最小寄存器集合：
//! RBR/THR、IER、IIR/FCR、LCR（含 DLAB）、MCR、LSR、MSR、SCR、DLL/DLM。

use std::collections::VecDeque;
use std::io::{self, Write};

/// COM1 的 PIO 基址。
pub const SERIAL_PORT_BASE: u16 = 0x3f8;
/// 串口占用的 PIO 端口数（0x3f8..=0x3ff）。
pub const SERIAL_PORT_SIZE: u16 = 8;

// 寄存器偏移（相对基址）。
const RBR: u16 = 0; // 接收缓冲（读，DLAB=0）
const THR: u16 = 0; // 发送保持（写，DLAB=0）
const DLL: u16 = 0; // 波特率除数低字节（DLAB=1）
const IER: u16 = 1; // 中断使能（DLAB=0）
const DLM: u16 = 1; // 波特率除数高字节（DLAB=1）
const IIR: u16 = 2; // 中断标识（读）
const FCR: u16 = 2; // FIFO 控制（写）
const LCR: u16 = 3; // 线路控制
const MCR: u16 = 4; // Modem 控制
const LSR: u16 = 5; // 线路状态
const MSR: u16 = 6; // Modem 状态
const SCR: u16 = 7; // 暂存寄存器（16550A 探测用）

// LCR 位。
const LCR_DLAB: u8 = 0x80;

// IER 位。
const IER_THRI: u8 = 0x02; // THR 空中断使能

// LSR 位。
const LSR_DR: u8 = 0x01; // 接收数据就绪
const LSR_THRE: u8 = 0x20; // 发送保持寄存器空
const LSR_TEMT: u8 = 0x40; // 发送器空

// IIR：无中断挂起。
const IIR_NO_INT: u8 = 0x01;
// IIR：THR 空中断挂起（发送方向恒瞬时完成，故只由 IER.THRI 决定）。
const IIR_THRE: u8 = 0x02;

/// COM1 在 in-kernel irqchip（8259A PIC）上的 IRQ 号。
pub const SERIAL_IRQ: u32 = 4;

// MSR：上报 CTS | DSR | DCD（输入对端恒在线）。
const MSR_DEFAULT: u8 = 0xb0;

// MCR 位。
const MCR_LOOP: u8 = 0x10; // 环回模式：TX 内部回送到 RX（8250 驱动自检依赖）

/// 最小 16550A UART。
///
/// 泛型 `W` 是 guest 输出的去向（`Vm` 中为 `io::Stdout`，测试中可为 `Vec<u8>`）。
pub struct Serial<W: Write> {
    out: W,
    rx: VecDeque<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
}

impl Serial<io::Stdout> {
    /// 创建输出打到 host stdout 的串口。
    pub fn new_stdout() -> Self {
        Serial::new(io::stdout())
    }
}

impl<W: Write> Serial<W> {
    /// 以指定输出 sink 创建串口。
    pub fn new(out: W) -> Self {
        Serial {
            out,
            rx: VecDeque::new(),
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0,
            dlm: 0,
        }
    }

    /// 向串口注入输入字节（输入方向接口）。
    ///
    /// M0 不在 run 循环中接线（host stdin → 注入留给后续），但 guest 侧
    /// 读 RBR / LSR 的路径已可用；注入的输入不会触发 guest 中断。
    pub fn enqueue_input(&mut self, data: &[u8]) {
        self.rx.extend(data.iter().copied());
    }

    /// 处理 guest 的 PIO 写（`offset` 为相对 `SERIAL_PORT_BASE` 的偏移）。
    pub fn write(&mut self, offset: u16, data: &[u8]) -> io::Result<()> {
        for &byte in data {
            self.write_byte(offset, byte)?;
        }
        Ok(())
    }

    fn write_byte(&mut self, offset: u16, byte: u8) -> io::Result<()> {
        let dlab = self.lcr & LCR_DLAB != 0;
        match (offset, dlab) {
            (THR, false) => {
                if self.mcr & MCR_LOOP != 0 {
                    // 环回模式：字节内部回送到接收队列（8250 驱动的
                    // loopback 自检写 256 字节再读回校验）。
                    self.rx.push_back(byte);
                } else {
                    self.out.write_all(&[byte])?;
                    self.out.flush()?;
                }
            }
            (DLL, true) => self.dll = byte,
            (IER, false) => self.ier = byte,
            (DLM, true) => self.dlm = byte,
            // FCR：FIFO 始终视为已清空，忽略写入值。
            (FCR, _) => {}
            (LCR, _) => self.lcr = byte,
            (MCR, _) => self.mcr = byte,
            (SCR, _) => self.scr = byte,
            // 其余偏移（LSR/MSR/IIR 只读）写入忽略。
            _ => {}
        }
        Ok(())
    }

    /// 处理 guest 的 PIO 读，返回读到的字节。
    pub fn read(&mut self, offset: u16) -> u8 {
        let dlab = self.lcr & LCR_DLAB != 0;
        match (offset, dlab) {
            (RBR, false) => self.rx.pop_front().unwrap_or(0),
            (DLL, true) => self.dll,
            (IER, false) => self.ier,
            (DLM, true) => self.dlm,
            (IIR, _) => {
                // 发送瞬时完成（THRE 恒 1）：THRI 使能即视为 THR 空中断挂起。
                if self.ier & IER_THRI != 0 {
                    IIR_THRE
                } else {
                    IIR_NO_INT
                }
            }
            (LCR, _) => self.lcr,
            (MCR, _) => self.mcr,
            (LSR, _) => {
                // 发送方向永远就绪；接收方向按是否有缓冲输入上报 DR。
                let dr = if self.rx.is_empty() { 0 } else { LSR_DR };
                LSR_THRE | LSR_TEMT | dr
            }
            (MSR, _) => MSR_DEFAULT,
            (SCR, _) => self.scr,
            _ => 0,
        }
    }

    /// 串口 IRQ 线的当前电平（电平触发）：发送恒瞬时完成，THRE 中断条件
    /// 等价于 IER.THRI 是否使能。`Vm` 在每次寄存器访问后据此升降 IRQ4。
    pub fn irq_level(&self) -> bool {
        self.ier & IER_THRI != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_serial() -> Serial<Vec<u8>> {
        Serial::new(Vec::new())
    }

    #[test]
    fn test_thr_writes_to_output() {
        let mut serial = new_serial();
        serial.write(THR, b"hello").unwrap();
        assert_eq!(b"hello", serial.out.as_slice());
        // 输出后 LSR 仍上报发送就绪。
        assert_eq!(LSR_THRE | LSR_TEMT, serial.read(LSR));
    }

    #[test]
    fn test_dlab_switches_dll_dlm() {
        let mut serial = new_serial();
        // 置 DLAB，写 offset 0/1 落到 DLL/DLM，不触发输出。
        serial.write(LCR, &[LCR_DLAB]).unwrap();
        serial.write(DLL, &[0x01]).unwrap();
        serial.write(DLM, &[0x00]).unwrap();
        assert!(serial.out.is_empty());
        assert_eq!(0x01, serial.read(DLL));
        assert_eq!(0x00, serial.read(DLM));
        // 清 DLAB 后 offset 0 恢复为 THR/RBR。
        serial.write(LCR, &[0x03]).unwrap();
        serial.write(THR, b"x").unwrap();
        assert_eq!(b"x", serial.out.as_slice());
        assert_eq!(0x03, serial.read(LCR));
    }

    #[test]
    fn test_scratch_register_loopback() {
        // Linux 8250 探测 16550A 依赖 SCR 可读写。
        let mut serial = new_serial();
        serial.write(SCR, &[0xa5]).unwrap();
        assert_eq!(0xa5, serial.read(SCR));
    }

    #[test]
    fn test_input_path() {
        let mut serial = new_serial();
        assert_eq!(0, serial.read(LSR) & LSR_DR);
        assert_eq!(0, serial.read(RBR));

        serial.enqueue_input(b"ab");
        assert_eq!(LSR_DR, serial.read(LSR) & LSR_DR);
        assert_eq!(b'a', serial.read(RBR));
        assert_eq!(b'b', serial.read(RBR));
        assert_eq!(0, serial.read(LSR) & LSR_DR);
    }

    #[test]
    fn test_iir_reports_no_interrupt() {
        let mut serial = new_serial();
        assert_eq!(IIR_NO_INT, serial.read(IIR));
        assert!(!serial.irq_level());
    }

    #[test]
    fn test_thre_interrupt_follows_thri() {
        let mut serial = new_serial();
        // 使能 THRI 后：IIR 报 THR 空中断挂起，IRQ 线拉高；
        // 清掉 THRI 后恢复无中断。Linux 8250 tty 写路径依赖此行为。
        serial.write(IER, &[IER_THRI]).unwrap();
        assert_eq!(IIR_THRE, serial.read(IIR));
        assert!(serial.irq_level());
        serial.write(IER, &[0]).unwrap();
        assert_eq!(IIR_NO_INT, serial.read(IIR));
        assert!(!serial.irq_level());
    }

    #[test]
    fn test_mcr_msrs() {
        let mut serial = new_serial();
        serial.write(MCR, &[0x0b]).unwrap();
        assert_eq!(0x0b, serial.read(MCR));
        assert_eq!(MSR_DEFAULT, serial.read(MSR));
    }

    #[test]
    fn test_loopback_mode() {
        let mut serial = new_serial();
        serial.write(MCR, &[MCR_LOOP]).unwrap();
        serial.write(THR, b"ab").unwrap();
        // 环回的字节不外发，而是进入接收队列
        assert!(serial.out.is_empty());
        assert_eq!(LSR_DR, serial.read(LSR) & LSR_DR);
        assert_eq!(b'a', serial.read(RBR));
        assert_eq!(b'b', serial.read(RBR));
        // 退出环回后恢复外发
        serial.write(MCR, &[0x0b]).unwrap();
        serial.write(THR, b"c").unwrap();
        assert_eq!(b"c", serial.out.as_slice());
    }
}

//! MMIO 设备框架：地址/IRQ 布局、`MmioDevice` 分发 trait 与 `DeviceManager`。
//!
//! 布局（见 ADR 0003）：设备窗口基址 [`MMIO_BASE`]，每设备寄存器窗 4KiB、
//! 步长 4KiB；IRQ（GSI）从 [`IRQ_BASE`] 起顺排（0~4 留给 in-kernel irqchip 的
//! 定时器/串口等 legacy 设备）。guest 经内核 cmdline `virtio_mmio.device=`
//! 声明设备（`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`），不引入 ACPI。

pub mod balloon;
pub mod blk;
pub mod mem;
pub mod net;
pub mod rng;
mod virtio_mmio;
pub mod vsock;

pub use balloon::Balloon;
pub use blk::Blk;
pub use mem::Mem;
pub use net::Net;
pub use rng::Rng;
pub use virtio_mmio::{
    VirtioDevice, VirtioMmio, ISR_CONFIG_CHANGE, ISR_USED_BUFFER, STATUS_ACKNOWLEDGE,
    STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FAILED, STATUS_FEATURES_OK, STATUS_NEEDS_RESET,
};
pub use vsock::Vsock;

/// virtio-mmio 设备窗口基址（3.25GiB，位于 3GiB 低端内存顶之上、4GiB 之下）。
pub const MMIO_BASE: u64 = 0xd000_0000;
/// 每个设备的寄存器窗大小（4KiB），同时是设备间的步长。
pub const MMIO_SLOT_SIZE: u64 = 0x1000;
/// 设备 IRQ（GSI）起始号：0~4 已被 legacy 设备占用（串口 COM1 = IRQ4）。
pub const IRQ_BASE: u32 = 5;
/// 设备数上限（窗口 4KiB × 32 = 128KiB，远小于到 4GiB 的可用空间）。
pub const MAX_DEVICES: usize = 32;

/// 设备框架错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// MMIO 设备数已达上限。
    #[error("MMIO 设备数已达上限（{MAX_DEVICES}）")]
    DeviceLimit,
    /// 创建 virtqueue 失败（如队列上限不是 2 的幂）。
    #[error("创建 virtqueue 失败: {0}")]
    Queue(virtio_queue::Error),
}

/// 可被挂到 guest MMIO 地址空间、按区间分发的设备。
pub trait MmioDevice: Send {
    /// 处理 guest 的 MMIO 读（`offset` 相对本设备窗口基址）。
    fn read(&mut self, offset: u64, data: &mut [u8]);
    /// 处理 guest 的 MMIO 写（`offset` 相对本设备窗口基址）。
    fn write(&mut self, offset: u64, data: &[u8]);
    /// 设备 IRQ 线的当前电平（电平触发）；默认无中断。
    fn irq_level(&self) -> bool {
        false
    }
}

/// 已注册设备及其分配到的窗口与 IRQ。
struct Slot {
    base: u64,
    irq: u32,
    dev: Box<dyn MmioDevice>,
}

/// MMIO 设备管理器：分配地址/IRQ、按地址分发访问、汇总 IRQ 电平。
///
/// `Vm` 持有一个实例；无设备时所有操作为空转（cmdline 为空串、
/// 分发读回 0、无 IRQ），行为与未引入设备框架前完全一致。
#[derive(Default)]
pub struct DeviceManager {
    slots: Vec<Slot>,
}

impl DeviceManager {
    /// 创建空的设备管理器。
    pub fn new() -> Self {
        DeviceManager::default()
    }

    /// 注册设备，返回分配到的 (窗口基址, IRQ)。
    pub fn register(&mut self, dev: Box<dyn MmioDevice>) -> Result<(u64, u32), Error> {
        if self.slots.len() >= MAX_DEVICES {
            return Err(Error::DeviceLimit);
        }
        let idx = self.slots.len();
        let base = MMIO_BASE + idx as u64 * MMIO_SLOT_SIZE;
        let irq = IRQ_BASE + idx as u32;
        self.slots.push(Slot { base, irq, dev });
        Ok((base, irq))
    }

    /// 生成追加到内核 cmdline 的设备声明（空格分隔；无设备时为空串）。
    ///
    /// 形如 `virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0001000:6`。
    pub fn cmdline_args(&self) -> String {
        self.slots
            .iter()
            .map(|s| format!("virtio_mmio.device=4K@{:#x}:{}", s.base, s.irq))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// 按地址分发 MMIO 读；落在空洞（未注册区间）的读返回 0，不 panic。
    pub fn read(&mut self, addr: u64, data: &mut [u8]) {
        match self.find_slot(addr) {
            Some(slot) => slot.dev.read(addr - slot.base, data),
            None => {
                tracing::debug!(addr, len = data.len(), "MMIO 读落在设备空洞，返回 0");
                data.fill(0);
            }
        }
    }

    /// 按地址分发 MMIO 写；落在空洞的写被忽略，不 panic。
    pub fn write(&mut self, addr: u64, data: &[u8]) {
        match self.find_slot(addr) {
            Some(slot) => slot.dev.write(addr - slot.base, data),
            None => tracing::debug!(addr, len = data.len(), "MMIO 写落在设备空洞，忽略"),
        }
    }

    /// 遍历所有设备的 (IRQ, 当前电平)，供 run 循环刷新 KVM IRQ 线。
    pub fn irq_lines(&self) -> impl Iterator<Item = (u32, bool)> + '_ {
        self.slots.iter().map(|s| (s.irq, s.dev.irq_level()))
    }

    fn find_slot(&mut self, addr: u64) -> Option<&mut Slot> {
        self.slots
            .iter_mut()
            .find(|s| (s.base..s.base + MMIO_SLOT_SIZE).contains(&addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 记录型桩设备：读填充固定字节、写记录最近一次内容、IRQ 电平可设。
    struct StubDev {
        fill: u8,
        irq: bool,
        last_write: Vec<u8>,
    }

    impl StubDev {
        fn new(fill: u8) -> Self {
            StubDev {
                fill,
                irq: false,
                last_write: Vec::new(),
            }
        }
    }

    impl MmioDevice for StubDev {
        fn read(&mut self, _offset: u64, data: &mut [u8]) {
            data.fill(self.fill);
        }

        fn write(&mut self, _offset: u64, data: &[u8]) {
            self.last_write = data.to_vec();
        }

        fn irq_level(&self) -> bool {
            self.irq
        }
    }

    #[test]
    fn test_register_assigns_addr_and_irq() {
        let mut mgr = DeviceManager::new();
        let (base0, irq0) = mgr.register(Box::new(StubDev::new(0))).unwrap();
        let (base1, irq1) = mgr.register(Box::new(StubDev::new(0))).unwrap();
        assert_eq!(MMIO_BASE, base0);
        assert_eq!(IRQ_BASE, irq0);
        assert_eq!(MMIO_BASE + MMIO_SLOT_SIZE, base1);
        assert_eq!(IRQ_BASE + 1, irq1);
    }

    #[test]
    fn test_device_limit() {
        let mut mgr = DeviceManager::new();
        for _ in 0..MAX_DEVICES {
            mgr.register(Box::new(StubDev::new(0))).unwrap();
        }
        assert!(matches!(
            mgr.register(Box::new(StubDev::new(0))),
            Err(Error::DeviceLimit)
        ));
    }

    #[test]
    fn test_cmdline_args() {
        // 无设备：空串（保证 M0 行为不变）。
        let mut mgr = DeviceManager::new();
        assert_eq!("", mgr.cmdline_args());

        mgr.register(Box::new(StubDev::new(0))).unwrap();
        mgr.register(Box::new(StubDev::new(0))).unwrap();
        assert_eq!(
            "virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0001000:6",
            mgr.cmdline_args()
        );
    }

    #[test]
    fn test_dispatch_read_write() {
        let mut mgr = DeviceManager::new();
        mgr.register(Box::new(StubDev::new(0xaa))).unwrap();
        mgr.register(Box::new(StubDev::new(0xbb))).unwrap();

        // 按窗口分发到对应设备。
        let mut buf = [0u8; 4];
        mgr.read(MMIO_BASE, &mut buf);
        assert_eq!([0xaa; 4], buf);
        mgr.read(MMIO_BASE + MMIO_SLOT_SIZE + 0x100, &mut buf);
        assert_eq!([0xbb; 4], buf);

        // 写也按窗口分发。
        mgr.write(MMIO_BASE + 8, &[1, 2, 3, 4]);
        mgr.write(MMIO_BASE + MMIO_SLOT_SIZE, &[5, 6]);
        // 窗口内最后一个字节仍属该设备。
        mgr.read(MMIO_BASE + MMIO_SLOT_SIZE - 1, &mut buf[..1]);
        assert_eq!([0xaa], buf[..1]);
    }

    #[test]
    fn test_hole_access_no_panic() {
        let mut mgr = DeviceManager::new();
        mgr.register(Box::new(StubDev::new(0xaa))).unwrap();

        // 窗口间空洞（本布局无间隙，用未注册的高地址与窗口前地址）。
        let mut buf = [0xffu8; 4];
        mgr.read(MMIO_BASE + 32 * MMIO_SLOT_SIZE, &mut buf);
        assert_eq!([0; 4], buf);
        mgr.read(MMIO_BASE - 0x1000, &mut buf);
        assert_eq!([0; 4], buf);
        mgr.write(MMIO_BASE + 32 * MMIO_SLOT_SIZE, &[1, 2, 3, 4]);
        mgr.write(0, &[0]);
    }

    #[test]
    fn test_irq_lines() {
        let mut mgr = DeviceManager::new();
        assert_eq!(0, mgr.irq_lines().count());

        let mut dev = StubDev::new(0);
        dev.irq = true;
        mgr.register(Box::new(dev)).unwrap();
        mgr.register(Box::new(StubDev::new(0))).unwrap();

        let lines: Vec<(u32, bool)> = mgr.irq_lines().collect();
        assert_eq!(vec![(IRQ_BASE, true), (IRQ_BASE + 1, false)], lines);
    }
}

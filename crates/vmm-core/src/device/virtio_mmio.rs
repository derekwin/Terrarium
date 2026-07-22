//! virtio-mmio v2 传输层：把 virtio 设备寄存器接口暴露到 guest MMIO 窗口。
//!
//! 寄存器布局与语义按 virtio spec 1.x「Virtio Over MMIO」（只实现 v2，
//! Version 寄存器恒为 2，不支持 legacy v1）。队列状态直接复用
//! rust-vmm `virtio_queue::Queue`（描述符链解析不重写，见 ADR 0003）。
//!
//! 中断模型：ISR bit0（used buffer）由设备 `queue_notify` 的返回位置位，
//! 设备自发中断位（如 config change 的 bit1）经 `pending_interrupts` 并入；
//! guest 写 InterruptACK 清位；IRQ 电平 = 合并后 ISR 非零。

use tracing::debug;
use virtio_bindings::bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::{Error, MmioDevice};

// 寄存器偏移（virtio-mmio v2，全部为 32 位寄存器）。
const REG_MAGIC: u64 = 0x000; // MagicValue R，"virt"
const REG_VERSION: u64 = 0x004; // Version R，v2 = 2
const REG_DEVICE_ID: u64 = 0x008; // DeviceID R
const REG_VENDOR_ID: u64 = 0x00c; // VendorID R
const REG_DEVICE_FEATURES: u64 = 0x010; // DeviceFeatures R（由 Sel 选高低 32 位）
const REG_DEVICE_FEATURES_SEL: u64 = 0x014; // DeviceFeaturesSel W
const REG_DRIVER_FEATURES: u64 = 0x020; // DriverFeatures W（由 Sel 选高低 32 位）
const REG_DRIVER_FEATURES_SEL: u64 = 0x024; // DriverFeaturesSel W
const REG_QUEUE_SEL: u64 = 0x030; // QueueSel W
const REG_QUEUE_NUM_MAX: u64 = 0x034; // QueueNumMax R
const REG_QUEUE_NUM: u64 = 0x038; // QueueNum W
const REG_QUEUE_READY: u64 = 0x044; // QueueReady RW
const REG_QUEUE_NOTIFY: u64 = 0x050; // QueueNotify W
const REG_INTERRUPT_STATUS: u64 = 0x060; // InterruptStatus R（ISR）
const REG_INTERRUPT_ACK: u64 = 0x064; // InterruptACK W
const REG_STATUS: u64 = 0x070; // Status RW
const REG_QUEUE_DESC_LOW: u64 = 0x080; // QueueDescLow W
const REG_QUEUE_DESC_HIGH: u64 = 0x084; // QueueDescHigh W
const REG_QUEUE_AVAIL_LOW: u64 = 0x090; // QueueAvailLow W
const REG_QUEUE_AVAIL_HIGH: u64 = 0x094; // QueueAvailHigh W
const REG_QUEUE_USED_LOW: u64 = 0x0a0; // QueueUsedLow W
const REG_QUEUE_USED_HIGH: u64 = 0x0a4; // QueueUsedHigh W
const REG_CONFIG_GENERATION: u64 = 0x0fc; // ConfigGeneration R
/// 设备配置空间起点（0x100 起，布局由具体设备定义）。
const REG_CONFIG: u64 = 0x100;

/// MagicValue 寄存器值："virt" 小端。
const MAGIC_VALUE: u32 = 0x7472_6976;
/// Version 寄存器值：只支持 v2。
const MMIO_VERSION: u32 = 2;

// Status 位（virtio spec 2.1 Device Status Field）。
/// 驱动已发现设备。
pub const STATUS_ACKNOWLEDGE: u32 = 1;
/// 驱动知道如何驱动设备。
pub const STATUS_DRIVER: u32 = 2;
/// 驱动初始化完成，设备可用。
pub const STATUS_DRIVER_OK: u32 = 4;
/// 特性协商完成。
pub const STATUS_FEATURES_OK: u32 = 8;
/// 设备要求复位（设备侧上报，M1 暂未使用）。
pub const STATUS_NEEDS_RESET: u32 = 64;
/// 设备或驱动判定失败（M1 暂未使用）。
pub const STATUS_FAILED: u32 = 128;

// ISR 位。
/// bit0：used buffer（队列有已完成的描述符）。
pub const ISR_USED_BUFFER: u32 = 1;
/// bit1：config change（设备配置变化）。
pub const ISR_CONFIG_CHANGE: u32 = 2;

/// 具体 virtio 设备与传输层的分界（Task 1 的 blk 等设备实现它）。
///
/// 传输层（[`VirtioMmio`]）负责寄存器、Status 握手、队列地址与 ISR；
/// 设备只关心自己的特性位、配置空间与队列数据处理。
pub trait VirtioDevice: Send {
    /// virtio spec 设备号：1=net, 2=blk, ...
    fn device_id(&self) -> u32;
    /// 设备特性位（不含传输层自动附加的 `VIRTIO_F_VERSION_1`）。
    fn features(&self) -> u64;
    /// virtqueue 数量。
    fn queue_count(&self) -> usize;
    /// 每个 virtqueue 的最大长度（须为 2 的幂）。
    fn queue_max_size(&self) -> u16;
    /// 读设备配置空间（`offset` 相对 0x100 的配置区起点）。
    fn read_config(&self, offset: u64, data: &mut [u8]);
    /// 写设备配置空间。
    fn write_config(&mut self, offset: u64, data: &[u8]);
    /// 队列被 kick（guest 写 QueueNotify）；返回需要置位的 ISR 位
    /// （通常有可用描述符被消费后置 [`ISR_USED_BUFFER`]）。
    fn queue_notify(&mut self, queue: usize) -> u32;
    /// 设备自发中断位（如 config change 的 [`ISR_CONFIG_CHANGE`]），
    /// 传输层每次读 ISR / 刷新 IRQ 电平时并入。
    fn pending_interrupts(&self) -> u32 {
        0
    }
    /// 复位设备（guest 写 Status=0 时由传输层调用）。
    fn reset(&mut self);
}

/// virtio-mmio v2 传输层，实现 [`MmioDevice`] 挂到 [`super::DeviceManager`]。
///
/// 持有全部 virtqueue（`virtio_queue::Queue`）与 guest 内存克隆（队列
/// `is_valid` 校验与后续描述符访问用）；具体设备经泛型 `D` 组合进来。
pub struct VirtioMmio<D: VirtioDevice> {
    device: D,
    mem: GuestMemoryMmap,
    queues: Vec<Queue>,
    /// 设备特性 | VIRTIO_F_VERSION_1（v2 传输层必须提供）。
    device_features: u64,
    /// 驱动接受的特性位（写 DriverFeatures 时记录）。
    driver_features: u64,
    features_sel: u32,
    driver_features_sel: u32,
    queue_sel: u32,
    /// 传输层持有的 ISR 位（bit0 由 queue_notify 置位、ACK 清除）；
    /// 设备自发位经 `pending_interrupts` 在读出/判电平时并入。
    isr: u32,
    status: u32,
}

impl<D: VirtioDevice> VirtioMmio<D> {
    /// 为设备创建传输层：`mem` 是 guest 内存（克隆传入，用于队列校验
    /// 与后续描述符访问）。
    pub fn new(device: D, mem: GuestMemoryMmap) -> Result<Self, Error> {
        let mut queues = Vec::with_capacity(device.queue_count());
        for _ in 0..device.queue_count() {
            queues.push(Queue::new(device.queue_max_size()).map_err(Error::Queue)?);
        }
        let device_features = device.features() | (1u64 << VIRTIO_F_VERSION_1);
        Ok(VirtioMmio {
            device,
            mem,
            queues,
            device_features,
            driver_features: 0,
            features_sel: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            isr: 0,
            status: 0,
        })
    }

    /// 读对齐 32 位寄存器（`offset` 已按 4 对齐）。
    fn read_reg(&mut self, offset: u64) -> u32 {
        match offset {
            REG_MAGIC => MAGIC_VALUE,
            REG_VERSION => MMIO_VERSION,
            REG_DEVICE_ID => self.device.device_id(),
            REG_VENDOR_ID => 0, // 无厂商 ID 需求；Linux virtio-mmio 驱动不检查
            REG_DEVICE_FEATURES => match self.features_sel {
                0 => self.device_features as u32,
                1 => (self.device_features >> 32) as u32,
                _ => 0,
            },
            REG_QUEUE_NUM_MAX => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| u32::from(q.max_size())),
            REG_QUEUE_READY => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| u32::from(q.ready())),
            REG_INTERRUPT_STATUS => self.isr | self.device.pending_interrupts(),
            REG_STATUS => self.status,
            REG_CONFIG_GENERATION => 0, // config change 由 Task 3（virtio-mem）引入
            _ => {
                debug!(offset, "读未实现/只写的 virtio-mmio 寄存器，返回 0");
                0
            }
        }
    }

    /// 写对齐 32 位寄存器（`offset` 已按 4 对齐）。
    fn write_reg(&mut self, offset: u64, v: u32) {
        match offset {
            REG_DEVICE_FEATURES_SEL => self.features_sel = v,
            REG_DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features = (self.driver_features & !0xffff_ffff) | u64::from(v);
                } else if self.driver_features_sel == 1 {
                    self.driver_features =
                        (self.driver_features & 0xffff_ffff) | (u64::from(v) << 32);
                }
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = v,
            REG_QUEUE_SEL => self.queue_sel = v,
            REG_QUEUE_NUM => {
                if let Some(q) = self.selected_queue() {
                    // set_size 内部校验 2 的幂与上限，非法值仅记录日志不生效。
                    q.set_size(v as u16);
                }
            }
            REG_QUEUE_READY => {
                let sel = self.queue_sel;
                // 直接按字段借用（self.queues 与 self.mem 是不相交字段），
                // 以便 is_valid 能同时访问 guest 内存。
                if let Some(q) = self.queues.get_mut(sel as usize) {
                    if v == 1 {
                        q.set_ready(true);
                        // 地址/长度非法（越出 guest 内存、未对齐等）的队列
                        // 不允许 ready：读回 0，驱动按规范视为配置失败。
                        if !q.is_valid(&self.mem) {
                            debug!(queue = sel, "virtqueue 校验失败，拒绝 ready");
                            q.set_ready(false);
                        }
                    } else {
                        q.set_ready(false);
                    }
                }
            }
            REG_QUEUE_NOTIFY => {
                // 写入值即队列索引；设备返回需要置位的 ISR 位。
                self.isr |= self.device.queue_notify(v as usize);
            }
            REG_INTERRUPT_ACK => self.isr &= !v,
            REG_STATUS => {
                if v == 0 {
                    self.reset();
                } else {
                    self.set_status(v);
                }
            }
            REG_QUEUE_DESC_LOW => {
                if let Some(q) = self.selected_queue() {
                    q.set_desc_table_address(Some(v), None);
                }
            }
            REG_QUEUE_DESC_HIGH => {
                if let Some(q) = self.selected_queue() {
                    q.set_desc_table_address(None, Some(v));
                }
            }
            REG_QUEUE_AVAIL_LOW => {
                if let Some(q) = self.selected_queue() {
                    q.set_avail_ring_address(Some(v), None);
                }
            }
            REG_QUEUE_AVAIL_HIGH => {
                if let Some(q) = self.selected_queue() {
                    q.set_avail_ring_address(None, Some(v));
                }
            }
            REG_QUEUE_USED_LOW => {
                if let Some(q) = self.selected_queue() {
                    q.set_used_ring_address(Some(v), None);
                }
            }
            REG_QUEUE_USED_HIGH => {
                if let Some(q) = self.selected_queue() {
                    q.set_used_ring_address(None, Some(v));
                }
            }
            _ => debug!(offset, v, "写未实现/只读的 virtio-mmio 寄存器，忽略"),
        }
    }

    fn selected_queue(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// 写入新的 Status；对 FEATURES_OK 做协商校验（驱动接受了设备不支持的
    /// 特性位时，读回不带 FEATURES_OK，驱动据此判定协商失败）。
    fn set_status(&mut self, v: u32) {
        let mut new = v;
        if v & STATUS_FEATURES_OK != 0 && self.driver_features & !self.device_features != 0 {
            debug!(
                driver = self.driver_features,
                device = self.device_features,
                "驱动接受了不支持的特性位，拒绝 FEATURES_OK"
            );
            new &= !STATUS_FEATURES_OK;
        }
        self.status = new;
        if self.status & STATUS_FEATURES_OK != 0 {
            // EVENT_IDX 协商结果下发到各队列（中断合并用）。
            let event_idx = self.driver_features & (1u64 << VIRTIO_RING_F_EVENT_IDX) != 0;
            for q in &mut self.queues {
                q.set_event_idx(event_idx);
            }
        }
    }

    /// guest 写 Status=0 触发的完整复位（规范 2.1.1：状态机回到初始）。
    fn reset(&mut self) {
        self.status = 0;
        self.driver_features = 0;
        self.features_sel = 0;
        self.driver_features_sel = 0;
        self.queue_sel = 0;
        self.isr = 0;
        for q in &mut self.queues {
            q.reset();
        }
        self.device.reset();
    }
}

impl<D: VirtioDevice> MmioDevice for VirtioMmio<D> {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= REG_CONFIG {
            // 配置空间：布局由设备定义，访问粒度不限，直通给设备。
            self.device.read_config(offset - REG_CONFIG, data);
            return;
        }
        // 寄存器区按 32 位寄存器取值，兼容部分读（规范要求 32 位对齐访问，
        // Linux 驱动始终如此；这里逐字节截取以容忍其他粒度）。
        for (i, byte) in data.iter_mut().enumerate() {
            let off = offset + i as u64;
            *byte = (self.read_reg(off & !3) >> ((off & 3) * 8)) as u8;
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= REG_CONFIG {
            self.device.write_config(offset - REG_CONFIG, data);
            return;
        }
        if !offset.is_multiple_of(4) || data.len() != 4 {
            debug!(
                offset,
                len = data.len(),
                "忽略非 32 位对齐的 virtio-mmio 寄存器写"
            );
            return;
        }
        let v = u32::from_le_bytes(data.try_into().expect("已校验长度为 4"));
        self.write_reg(offset, v);
    }

    fn irq_level(&self) -> bool {
        self.isr | self.device.pending_interrupts() != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestAddress;

    /// 记录型 mock 设备：1 条队列、8 字节配置空间、notify 固定上报 used buffer。
    struct MockDevice {
        config: [u8; 8],
        notified: Vec<usize>,
        reset_count: u32,
        pending: u32,
    }

    impl MockDevice {
        fn new() -> Self {
            MockDevice {
                config: [0; 8],
                notified: Vec::new(),
                reset_count: 0,
                pending: 0,
            }
        }
    }

    impl VirtioDevice for MockDevice {
        fn device_id(&self) -> u32 {
            42
        }
        fn features(&self) -> u64 {
            1 << 5 // 随便一个设备特性位
        }
        fn queue_count(&self) -> usize {
            1
        }
        fn queue_max_size(&self) -> u16 {
            256
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            let start = offset as usize;
            data.copy_from_slice(&self.config[start..start + data.len()]);
        }
        fn write_config(&mut self, offset: u64, data: &[u8]) {
            let start = offset as usize;
            self.config[start..start + data.len()].copy_from_slice(data);
        }
        fn queue_notify(&mut self, queue: usize) -> u32 {
            self.notified.push(queue);
            ISR_USED_BUFFER
        }
        fn pending_interrupts(&self) -> u32 {
            self.pending
        }
        fn reset(&mut self) {
            self.reset_count += 1;
        }
    }

    fn new_mmio() -> VirtioMmio<MockDevice> {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 128 << 20)]).unwrap();
        VirtioMmio::new(MockDevice::new(), mem).unwrap()
    }

    fn read32(mmio: &mut VirtioMmio<MockDevice>, offset: u64) -> u32 {
        let mut buf = [0u8; 4];
        mmio.read(offset, &mut buf);
        u32::from_le_bytes(buf)
    }

    fn write32(mmio: &mut VirtioMmio<MockDevice>, offset: u64, v: u32) {
        mmio.write(offset, &v.to_le_bytes());
    }

    /// 驱动握手走到 FEATURES_OK 之前的公共前缀。
    fn handshake_prefix(mmio: &mut VirtioMmio<MockDevice>) {
        write32(mmio, REG_STATUS, STATUS_ACKNOWLEDGE);
        write32(mmio, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        // 读设备特性并接受（VERSION_1 必带，设备位 5 也接受）。
        write32(mmio, REG_DRIVER_FEATURES_SEL, 0);
        write32(mmio, REG_DRIVER_FEATURES, 1 << 5);
        write32(mmio, REG_DRIVER_FEATURES_SEL, 1);
        write32(mmio, REG_DRIVER_FEATURES, 1); // bit32 = VERSION_1
        write32(
            mmio,
            REG_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
        );
    }

    /// 把队列 0 配好并置 ready。
    fn setup_queue(mmio: &mut VirtioMmio<MockDevice>) {
        write32(mmio, REG_QUEUE_SEL, 0);
        write32(mmio, REG_QUEUE_NUM, 256);
        write32(mmio, REG_QUEUE_DESC_LOW, 0x10000);
        write32(mmio, REG_QUEUE_DESC_HIGH, 0);
        write32(mmio, REG_QUEUE_AVAIL_LOW, 0x20000);
        write32(mmio, REG_QUEUE_AVAIL_HIGH, 0);
        write32(mmio, REG_QUEUE_USED_LOW, 0x30000);
        write32(mmio, REG_QUEUE_USED_HIGH, 0);
        write32(mmio, REG_QUEUE_READY, 1);
    }

    #[test]
    fn test_id_registers() {
        let mut mmio = new_mmio();
        assert_eq!(MAGIC_VALUE, read32(&mut mmio, REG_MAGIC));
        assert_eq!(2, read32(&mut mmio, REG_VERSION));
        assert_eq!(42, read32(&mut mmio, REG_DEVICE_ID));
        assert_eq!(0, read32(&mut mmio, REG_VENDOR_ID));
        assert_eq!(0, read32(&mut mmio, REG_CONFIG_GENERATION));
    }

    #[test]
    fn test_device_features_include_version_1() {
        let mut mmio = new_mmio();
        write32(&mut mmio, REG_DEVICE_FEATURES_SEL, 0);
        assert_eq!(1 << 5, read32(&mut mmio, REG_DEVICE_FEATURES));
        write32(&mut mmio, REG_DEVICE_FEATURES_SEL, 1);
        assert_eq!(1, read32(&mut mmio, REG_DEVICE_FEATURES)); // bit32 = VERSION_1
    }

    #[test]
    fn test_full_handshake_notify_ack() {
        let mut mmio = new_mmio();
        handshake_prefix(&mut mmio);
        let status = read32(&mut mmio, REG_STATUS);
        assert_eq!(
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
            status
        );

        // 队列设置：QueueNumMax 来自设备，ready 后读回 1。
        write32(&mut mmio, REG_QUEUE_SEL, 0);
        assert_eq!(256, read32(&mut mmio, REG_QUEUE_NUM_MAX));
        setup_queue(&mut mmio);
        assert_eq!(1, read32(&mut mmio, REG_QUEUE_READY));

        // DRIVER_OK 完成握手。
        write32(
            &mut mmio,
            REG_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );

        // notify → 设备 queue_notify 被调用，ISR bit0 置位，IRQ 拉高。
        assert!(!mmio.irq_level());
        write32(&mut mmio, REG_QUEUE_NOTIFY, 0);
        assert_eq!(vec![0], mmio.device.notified);
        assert_eq!(ISR_USED_BUFFER, read32(&mut mmio, REG_INTERRUPT_STATUS));
        assert!(mmio.irq_level());

        // ACK 清位，IRQ 落下。
        write32(&mut mmio, REG_INTERRUPT_ACK, ISR_USED_BUFFER);
        assert_eq!(0, read32(&mut mmio, REG_INTERRUPT_STATUS));
        assert!(!mmio.irq_level());
    }

    #[test]
    fn test_features_ok_rejected_on_unsupported_bits() {
        let mut mmio = new_mmio();
        write32(&mut mmio, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        // 驱动接受了设备不支持的位（bit 10）。
        write32(&mut mmio, REG_DRIVER_FEATURES_SEL, 0);
        write32(&mut mmio, REG_DRIVER_FEATURES, 1 << 10);
        write32(
            &mut mmio,
            REG_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
        );
        assert_eq!(0, read32(&mut mmio, REG_STATUS) & STATUS_FEATURES_OK);
    }

    #[test]
    fn test_queue_ready_rejected_when_invalid() {
        let mut mmio = new_mmio();
        write32(&mut mmio, REG_QUEUE_SEL, 0);
        // 地址越出 guest 内存（128MiB）的队列不允许 ready。
        write32(&mut mmio, REG_QUEUE_NUM, 256);
        write32(&mut mmio, REG_QUEUE_DESC_LOW, 0x8000_0000);
        write32(&mut mmio, REG_QUEUE_AVAIL_LOW, 0x8000_1000);
        write32(&mut mmio, REG_QUEUE_USED_LOW, 0x8000_2000);
        write32(&mut mmio, REG_QUEUE_READY, 1);
        assert_eq!(0, read32(&mut mmio, REG_QUEUE_READY));
    }

    #[test]
    fn test_status_zero_resets_everything() {
        let mut mmio = new_mmio();
        handshake_prefix(&mut mmio);
        setup_queue(&mut mmio);
        write32(&mut mmio, REG_QUEUE_NOTIFY, 0);
        assert!(mmio.irq_level());

        write32(&mut mmio, REG_STATUS, 0);
        assert_eq!(0, read32(&mut mmio, REG_STATUS));
        assert_eq!(0, read32(&mut mmio, REG_INTERRUPT_STATUS));
        assert!(!mmio.irq_level());
        assert_eq!(0, read32(&mut mmio, REG_QUEUE_READY));
        assert_eq!(1, mmio.device.reset_count);
    }

    #[test]
    fn test_config_space_passthrough() {
        let mut mmio = new_mmio();
        mmio.write(REG_CONFIG, &[1, 2, 3, 4]);
        mmio.write(REG_CONFIG + 4, &[5, 6, 7, 8]);
        assert_eq!([1, 2, 3, 4, 5, 6, 7, 8], mmio.device.config);
        let mut buf = [0u8; 8];
        mmio.read(REG_CONFIG, &mut buf);
        assert_eq!([1, 2, 3, 4, 5, 6, 7, 8], buf);
    }

    #[test]
    fn test_pending_interrupts_merged() {
        let mut mmio = new_mmio();
        // 设备自发中断位（如 config change）并入 ISR 与 IRQ 电平。
        mmio.device.pending = ISR_CONFIG_CHANGE;
        assert_eq!(ISR_CONFIG_CHANGE, read32(&mut mmio, REG_INTERRUPT_STATUS));
        assert!(mmio.irq_level());
        // ACK 清不掉设备自发位（由设备自己清）。
        write32(&mut mmio, REG_INTERRUPT_ACK, ISR_CONFIG_CHANGE);
        assert_eq!(ISR_CONFIG_CHANGE, read32(&mut mmio, REG_INTERRUPT_STATUS));
        assert!(mmio.irq_level());
    }

    #[test]
    fn test_unaligned_and_partial_access_no_panic() {
        let mut mmio = new_mmio();
        // 部分读：逐字节从所在寄存器截取。
        let mut buf = [0u8; 2];
        mmio.read(REG_MAGIC + 1, &mut buf);
        assert_eq!([0x69, 0x72], buf); // "ri"（"virt" 的第 2、3 字节）
                                       // 非对齐/非 4 字节写被忽略，不 panic。
        mmio.write(REG_STATUS + 1, &[0xff; 4]);
        mmio.write(REG_STATUS, &[1, 2]);
        assert_eq!(0, read32(&mut mmio, REG_STATUS));
        // 非法队列索引的访问不 panic。
        write32(&mut mmio, REG_QUEUE_SEL, 7);
        assert_eq!(0, read32(&mut mmio, REG_QUEUE_NUM_MAX));
        write32(&mut mmio, REG_QUEUE_NUM, 16);
        write32(&mut mmio, REG_QUEUE_READY, 1);
        assert_eq!(0, read32(&mut mmio, REG_QUEUE_READY));
    }
}

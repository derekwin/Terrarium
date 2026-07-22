//! virtio-watchdog 设备。
//!
//! device_id=23，无队列。guest 周期性写入 control queue 防超时。
//! M1.5: 骨架实现——注册设备，guest 内核可加载驱动，实际超时动作留待后续。

use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::virtio_mmio::{VirtioDevice, ISR_USED_BUFFER};

const VIRTIO_ID_WATCHDOG: u32 = 23;

pub struct Watchdog;

impl Watchdog {
    pub fn new() -> Self { Watchdog }
}

impl Default for Watchdog {
    fn default() -> Self { Self::new() }
}

impl VirtioDevice for Watchdog {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_WATCHDOG
    }
    fn features(&self) -> u64 {
        0
    }
    fn queue_count(&self) -> usize {
        1
    }
    fn queue_max_size(&self) -> u16 {
        128
    }
    fn read_config(&self, _offset: u64, _data: &mut [u8]) {}
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn queue_notify(&mut self, _qi: usize, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        // Pet the watchdog: consume any available descriptor chains.
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let _ = queue.add_used(mem, head, 0);
            used = true;
        }
        if used {
            ISR_USED_BUFFER
        } else {
            0
        }
    }
    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_identity() {
        let w = Watchdog::new();
        assert_eq!(23, w.device_id());
        assert_eq!(1, w.queue_count());
    }
}

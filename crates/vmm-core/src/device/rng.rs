//! virtio-rng 设备（entropy source）。
//!
//! device_id=4，单队列。guest 提交 request → host 从 /dev/urandom 填充随机字节。
//! 解决 Ubuntu cloud image 启动时因缺少熵源而卡在 systemd-random-seed 的问题。

use std::fs::File;
use std::io::{self, Read};

use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_USED_BUFFER};

const VIRTIO_ID_RNG: u32 = 4;

pub struct Rng {
    urandom: File,
}

impl Rng {
    pub fn new() -> io::Result<Self> {
        Ok(Rng {
            urandom: File::open("/dev/urandom")?,
        })
    }

    fn process_queue(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        let mut used_any = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
            let mut total = 0u32;
            for desc in &descs {
                if !desc.is_write_only() {
                    continue;
                }
                let len = desc.len() as usize;
                let mut buf = vec![0u8; len];
                if self.urandom.read_exact(&mut buf).is_ok() {
                    let _ = mem.write_slice(&buf, desc.addr());
                    total += len as u32;
                }
            }
            let _ = queue.add_used(mem, head, total);
            used_any = true;
        }
        if used_any {
            ISR_USED_BUFFER
        } else {
            0
        }
    }
}

impl VirtioDevice for Rng {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_RNG
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
        self.process_queue(queue, mem)
    }
    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_identity() -> io::Result<()> {
        let r = Rng::new()?;
        assert_eq!(4, r.device_id());
        assert_eq!(1, r.queue_count());
        Ok(())
    }
}

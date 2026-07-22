//! virtio-balloon 设备（参考 Dragonball balloon.rs，Apache-2.0）。
//!
//! device_id=5，inflate/deflate 双队列。features=VIRTIO_BALLOON_F_DEFLATE_ON_OOM。
//! config space：num_pages(u32) + actual(u32)。
//! inflate: guest 报告的 PFN → host 侧 MADV_DONTNEED 回收。
//! deflate: guest 报告的 PFN → host 侧 MADV_WILLNEED 预热。

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_CONFIG_CHANGE, ISR_USED_BUFFER};

const VIRTIO_ID_BALLOON: u32 = 5;
const VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u64 = 1 << 2;

const INFLATE_QUEUE: usize = 0;
const DEFLATE_QUEUE: usize = 1;
const PFN_SHIFT: u64 = 12;
const PAGE_SIZE: u64 = 1 << PFN_SHIFT;

#[repr(C, packed)]
struct BalloonConfig {
    num_pages: u32,
    actual: u32,
}

pub struct Balloon {
    config: BalloonConfig,
    config_changed: Arc<AtomicBool>,
}

impl Default for Balloon {
    fn default() -> Self {
        Self::new()
    }
}

impl Balloon {
    pub fn new() -> Self {
        Balloon {
            config: BalloonConfig {
                num_pages: 0,
                actual: 0,
            },
            config_changed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn madvise(addr: *mut std::ffi::c_void, len: usize, advice: i32) {
        unsafe {
            libc::madvise(addr, len, advice);
        }
    }

    fn process_queue(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap, advise: i32) -> u32 {
        let mut used_any = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
            let mut pages = 0u32;

            for desc in &descs {
                if desc.is_write_only() {
                    continue;
                }
                let addr = desc.addr();
                let len = desc.len() as usize;
                if !len.is_multiple_of(4) {
                    continue;
                }
                let mut offset = 0usize;
                while offset + 4 <= len {
                    let pfn: u32 = match mem.read_obj(GuestAddress(addr.0 + offset as u64)) {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let guest_addr = (pfn as u64) << PFN_SHIFT;
                    if let Ok(host_addr) = mem.get_host_address(GuestAddress(guest_addr)) {
                        Self::madvise(
                            host_addr as *mut std::ffi::c_void,
                            PAGE_SIZE as usize,
                            advise,
                        );
                    }
                    pages += 1;
                    offset += 4;
                }
            }

            if advise == libc::MADV_DONTNEED {
                self.config.actual = self.config.actual.wrapping_add(pages);
            } else {
                self.config.actual = self.config.actual.saturating_sub(pages);
            }
            let _ = queue.add_used(mem, head, 0);
            used_any = true;
        }
        if used_any {
            ISR_USED_BUFFER
        } else {
            0
        }
    }
}

impl VirtioDevice for Balloon {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_BALLOON
    }
    fn features(&self) -> u64 {
        VIRTIO_BALLOON_F_DEFLATE_ON_OOM
    }
    fn queue_count(&self) -> usize {
        2
    }
    fn queue_max_size(&self) -> u16 {
        128
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &self.config as *const BalloonConfig as *const u8,
                std::mem::size_of::<BalloonConfig>(),
            )
        };
        let start = offset as usize;
        let end = (start + data.len()).min(bytes.len());
        data[..end - start].copy_from_slice(&bytes[start..end]);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        if offset == 0 && data.len() == 4 {
            self.config.num_pages = u32::from_le_bytes(data.try_into().unwrap());
            self.config_changed.store(true, Ordering::SeqCst);
        }
    }

    fn queue_notify(&mut self, qi: usize, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        match qi {
            INFLATE_QUEUE => self.process_queue(queue, mem, libc::MADV_DONTNEED),
            DEFLATE_QUEUE => self.process_queue(queue, mem, libc::MADV_WILLNEED),
            _ => 0,
        }
    }

    fn pending_interrupts(&self) -> u32 {
        if self.config_changed.swap(false, Ordering::SeqCst) {
            ISR_CONFIG_CHANGE
        } else {
            0
        }
    }

    fn reset(&mut self) {
        self.config = BalloonConfig {
            num_pages: 0,
            actual: 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_identity() {
        let b = Balloon::new();
        assert_eq!(5, b.device_id());
        assert!(b.features() > 0);
        assert_eq!(2, b.queue_count());
    }
    #[test]
    fn test_config() {
        let mut b = Balloon::new();
        b.write_config(0, &128u32.to_le_bytes());
        let mut buf = [0u8; 4];
        b.read_config(0, &mut buf);
        assert_eq!(128u32.to_le_bytes(), buf);
    }
    #[test]
    fn test_default() {
        let _b = Balloon::default();
    }
}

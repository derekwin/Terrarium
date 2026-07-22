//! virtio-blk 设备（M1 Task 1）。
//!
//! device_id=2，单队列（size 128），features=VIRTIO_F_VERSION_1（传输层自动附加）
//! + VIRTIO_BLK_F_FLUSH。
//!
//! 后端为宿主普通文件（`std::os::unix::fs::FileExt`），
//! capacity = 文件长度 / 512（扇区数）。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_CONFIG_CHANGE, ISR_USED_BUFFER};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const VIRTIO_BLK_F_DISCARD: u64 = 1 << 13;

pub struct Blk {
    file: File,
    capacity: Arc<AtomicU64>,
    config_changed: Arc<AtomicBool>,
}

impl Blk {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let file_len = file.metadata()?.len();
        Ok(Blk {
            file,
            capacity: Arc::new(AtomicU64::new(file_len / 512)),
            config_changed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn capacity_arc(&self) -> Arc<AtomicU64> {
        self.capacity.clone()
    }
    pub fn config_changed_arc(&self) -> Arc<AtomicBool> {
        self.config_changed.clone()
    }

    pub fn resize(&self, new_bytes: u64) {
        self.capacity.store(new_bytes / 512, Ordering::SeqCst);
        self.config_changed.store(true, Ordering::SeqCst);
    }

    fn process_one(&mut self, chain: DescriptorChain<&GuestMemoryMmap>) -> io::Result<usize> {
        let mem = chain.memory().clone();
        let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();

        if descs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "空描述符链"));
        }

        let header_addr = descs[0].addr();
        let mut header_buf = [0u8; 16];
        mem.read_slice(&mut header_buf, header_addr)
            .map_err(io::Error::other)?;
        let request_type = u32::from_le_bytes(header_buf[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(header_buf[8..16].try_into().unwrap());

        let status_idx = descs
            .iter()
            .rposition(|d| d.is_write_only())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "缺少状态描述符"))?;
        let status_addr = descs[status_idx].addr();

        let write_status = |status: u8| -> io::Result<()> {
            mem.write_slice(&[status], status_addr)
                .map_err(io::Error::other)
        };

        if request_type != VIRTIO_BLK_T_IN
            && request_type != VIRTIO_BLK_T_OUT
            && request_type != VIRTIO_BLK_T_FLUSH
        {
            write_status(VIRTIO_BLK_S_UNSUPP)?;
            return Ok(0);
        }

        let result = match request_type {
            VIRTIO_BLK_T_IN => self.do_in(sector, &descs, status_idx, &mem),
            VIRTIO_BLK_T_OUT => self.do_out(sector, &descs, status_idx, &mem),
            VIRTIO_BLK_T_FLUSH => self.file.sync_data().map(|_| 0),
            _ => unreachable!(),
        };

        match result {
            Ok(n) => {
                write_status(VIRTIO_BLK_S_OK)?;
                Ok(n)
            }
            Err(e) => {
                write_status(VIRTIO_BLK_S_IOERR)?;
                Err(e)
            }
        }
    }

    fn do_in(
        &mut self,
        sector: u64,
        descs: &[virtio_queue::desc::split::Descriptor],
        status_idx: usize,
        mem: &GuestMemoryMmap,
    ) -> io::Result<usize> {
        let mut offset = sector
            .checked_mul(512)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "扇区号溢出"))?;
        let file_len = self.capacity.load(Ordering::SeqCst) * 512;
        let mut total = 0usize;

        for (i, desc) in descs.iter().enumerate() {
            if i == 0 || i == status_idx || !desc.is_write_only() {
                continue;
            }
            let len = desc.len() as usize;
            if len == 0 {
                continue;
            }
            if offset + len as u64 > file_len {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "超出磁盘边界"));
            }
            let mut buf = vec![0u8; len];
            self.file.read_exact_at(&mut buf, offset)?;
            mem.write_slice(&buf, desc.addr())
                .map_err(io::Error::other)?;
            offset += len as u64;
            total += len;
        }
        Ok(total)
    }

    fn do_out(
        &mut self,
        sector: u64,
        descs: &[virtio_queue::desc::split::Descriptor],
        status_idx: usize,
        mem: &GuestMemoryMmap,
    ) -> io::Result<usize> {
        let mut offset = sector
            .checked_mul(512)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "扇区号溢出"))?;
        let file_len = self.capacity.load(Ordering::SeqCst) * 512;
        let mut total = 0usize;

        for (i, desc) in descs.iter().enumerate() {
            if i == 0 || i == status_idx || desc.is_write_only() {
                continue;
            }
            let len = desc.len() as usize;
            if len == 0 {
                continue;
            }
            if offset + len as u64 > file_len {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "超出磁盘边界"));
            }
            let mut buf = vec![0u8; len];
            mem.read_slice(&mut buf, desc.addr())
                .map_err(io::Error::other)?;
            self.file.write_all_at(&buf, offset)?;
            offset += len as u64;
            total += len;
        }
        Ok(total)
    }
}

impl VirtioDevice for Blk {
    fn device_id(&self) -> u32 {
        2
    }

    fn features(&self) -> u64 {
        VIRTIO_BLK_F_FLUSH | VIRTIO_BLK_F_DISCARD
    }

    fn queue_count(&self) -> usize {
        1
    }

    fn queue_max_size(&self) -> u16 {
        128
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let cap_bytes = self.capacity.load(Ordering::SeqCst).to_le_bytes();
        let start = offset as usize;
        if start < 8 {
            let end = (start + data.len()).min(8);
            data[..end - start].copy_from_slice(&cap_bytes[start..end]);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn queue_notify(
        &mut self,
        _queue_index: usize,
        queue: &mut Queue,
        mem: &GuestMemoryMmap,
    ) -> u32 {
        let mut used_any = false;

        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let data_len = self.process_one(chain).unwrap_or_default();
            let _ = queue.add_used(mem, head, data_len as u32 + 1);
            used_any = true;
        }

        if used_any {
            ISR_USED_BUFFER
        } else {
            0
        }
    }

    fn pending_interrupts(&self) -> u32 {
        if self.config_changed.swap(false, Ordering::SeqCst) {
            ISR_CONFIG_CHANGE
        } else {
            0
        }
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use virtio_queue::QueueT;
    use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_file_path() -> std::path::PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("terra-blk-{}.img", n));
        p
    }

    /// 创建测试用的临时磁盘文件（1MiB，填充 0xAB）。
    fn make_disk(path: &std::path::Path) -> File {
        let f = File::create(path).unwrap();
        f.set_len(1024 * 1024).unwrap();
        f.sync_all().unwrap();
        f
    }

    /// 设置 guest 内存、队列和 blk 设备。
    fn setup(disk_path: &std::path::Path) -> (GuestMemoryMmap, Queue, Blk) {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 2 << 20)]).unwrap();
        let queue = Queue::new(128).unwrap();
        let blk = Blk::new(disk_path).unwrap();
        (mem, queue, blk)
    }

    /// 在 guest 内存中构建描述符表 / available ring / used ring，置队列就绪。
    fn build_request(mem: &GuestMemoryMmap, queue: &mut Queue, descriptors: &[(u64, u32, u16)]) {
        let desc_table = GuestAddress(0x1000);
        let avail_ring = GuestAddress(0x2000);
        let used_ring = GuestAddress(0x3000);

        let f_next = virtio_bindings::bindings::virtio_ring::VRING_DESC_F_NEXT as u16;

        for (i, &(addr, len, flags)) in descriptors.iter().enumerate() {
            let desc_addr = desc_table.unchecked_add((i * 16) as u64);
            let actual_flags = if i + 1 < descriptors.len() {
                flags | f_next
            } else {
                flags
            };
            let next = if i + 1 < descriptors.len() {
                (i + 1) as u16
            } else {
                0
            };

            mem.write_obj(addr, desc_addr).unwrap();
            mem.write_obj(len, desc_addr.unchecked_add(8)).unwrap();
            mem.write_obj(actual_flags, desc_addr.unchecked_add(12))
                .unwrap();
            mem.write_obj(next, desc_addr.unchecked_add(14)).unwrap();
        }

        mem.write_obj(0u16, avail_ring).unwrap();
        mem.write_obj(1u16, avail_ring.unchecked_add(2)).unwrap();
        mem.write_obj(0u16, avail_ring.unchecked_add(4)).unwrap();

        mem.write_obj(0u16, used_ring).unwrap();
        mem.write_obj(0u16, used_ring.unchecked_add(2)).unwrap();

        queue.set_size(128);
        queue.set_desc_table_address(Some(desc_table.0 as u32), None);
        queue.set_avail_ring_address(Some(avail_ring.0 as u32), None);
        queue.set_used_ring_address(Some(used_ring.0 as u32), None);
        queue.set_ready(true);
    }

    fn read_status(mem: &GuestMemoryMmap, status_addr: u64) -> u8 {
        let mut buf = [0u8; 1];
        mem.read_slice(&mut buf, GuestAddress(status_addr)).unwrap();
        buf[0]
    }

    #[test]
    fn test_device_identity() {
        let disk_path = test_file_path();
        let _disk = make_disk(&disk_path);
        let (_mem, _queue, blk) = setup(&disk_path);
        assert_eq!(2, blk.device_id());
        assert!(blk.features() & VIRTIO_BLK_F_FLUSH != 0);
        assert!(blk.features() & VIRTIO_BLK_F_DISCARD != 0);
        assert_eq!(1, blk.queue_count());
        assert_eq!(128, blk.queue_max_size());
    }

    #[test]
    fn test_config_capacity() {
        let disk_path = test_file_path();
        let _disk = make_disk(&disk_path);
        let (_mem, _queue, blk) = setup(&disk_path);
        assert_eq!(2048, blk.capacity.load(Ordering::SeqCst));

        let mut buf = [0xffu8; 8];
        blk.read_config(0, &mut buf);
        assert_eq!(2048u64.to_le_bytes(), buf);
    }

    #[test]
    fn test_in_request() {
        let disk_path = test_file_path();
        {
            let f = File::create(&disk_path).unwrap();
            f.write_all_at(b"HelloTerra!", 0).unwrap();
            f.set_len(1024 * 1024).unwrap();
            f.sync_all().unwrap();
        }

        let (mem, mut queue, mut blk) = setup(&disk_path);

        let header_addr = 0x4000u64;
        let data_addr = 0x5000u64;
        let status_addr = 0x6000u64;

        let header_bytes = [0u8; 16]; // type=IN(0), sector=0
        mem.write_slice(&header_bytes, GuestAddress(header_addr))
            .unwrap();

        let f_write = virtio_bindings::bindings::virtio_ring::VRING_DESC_F_WRITE as u16;

        build_request(
            &mem,
            &mut queue,
            &[
                (header_addr, 16, 0),
                (data_addr, 11, f_write),
                (status_addr, 1, f_write),
            ],
        );

        let isr = blk.queue_notify(0, &mut queue, &mem);
        assert_eq!(ISR_USED_BUFFER, isr);

        let mut buf = [0u8; 11];
        mem.read_slice(&mut buf, GuestAddress(data_addr)).unwrap();
        assert_eq!(b"HelloTerra!", &buf);
        assert_eq!(VIRTIO_BLK_S_OK, read_status(&mem, status_addr));
    }

    #[test]
    fn test_out_request() {
        let disk_path = test_file_path();
        let _disk = make_disk(&disk_path);

        let (mem, mut queue, mut blk) = setup(&disk_path);

        let header_addr = 0x4000u64;
        let data_addr = 0x5000u64;
        let status_addr = 0x6000u64;

        let mut header_bytes = [0u8; 16];
        header_bytes[0..4].copy_from_slice(&1u32.to_le_bytes()); // type=OUT
        mem.write_slice(&header_bytes, GuestAddress(header_addr))
            .unwrap();
        mem.write_slice(b"WriteTest!", GuestAddress(data_addr))
            .unwrap();

        let f_write = virtio_bindings::bindings::virtio_ring::VRING_DESC_F_WRITE as u16;

        build_request(
            &mem,
            &mut queue,
            &[
                (header_addr, 16, 0),
                (data_addr, 10, 0),
                (status_addr, 1, f_write),
            ],
        );

        let isr = blk.queue_notify(0, &mut queue, &mem);
        assert_eq!(ISR_USED_BUFFER, isr);

        let mut file_buf = [0u8; 10];
        blk.file.read_exact_at(&mut file_buf, 0).unwrap();
        assert_eq!(b"WriteTest!", &file_buf);
        assert_eq!(VIRTIO_BLK_S_OK, read_status(&mem, status_addr));
    }

    #[test]
    fn test_flush_request() {
        let disk_path = test_file_path();
        let _disk = make_disk(&disk_path);
        let (mem, mut queue, mut blk) = setup(&disk_path);

        let header_addr = 0x4000u64;
        let status_addr = 0x5000u64;

        let mut header_bytes = [0u8; 16];
        header_bytes[0..4].copy_from_slice(&4u32.to_le_bytes()); // FLUSH
        mem.write_slice(&header_bytes, GuestAddress(header_addr))
            .unwrap();

        let f_write = virtio_bindings::bindings::virtio_ring::VRING_DESC_F_WRITE as u16;

        build_request(
            &mem,
            &mut queue,
            &[(header_addr, 16, 0), (status_addr, 1, f_write)],
        );

        let isr = blk.queue_notify(0, &mut queue, &mem);
        assert_eq!(ISR_USED_BUFFER, isr);
        assert_eq!(VIRTIO_BLK_S_OK, read_status(&mem, status_addr));
    }

    #[test]
    fn test_out_of_bounds_returns_ioerr() {
        let disk_path = test_file_path();
        let _disk = make_disk(&disk_path);
        let (mem, mut queue, mut blk) = setup(&disk_path);

        let header_addr = 0x4000u64;
        let data_addr = 0x5000u64;
        let status_addr = 0x6000u64;

        let mut header_bytes = [0u8; 16];
        header_bytes[8..16].copy_from_slice(&3000u64.to_le_bytes()); // sector=3000
        mem.write_slice(&header_bytes, GuestAddress(header_addr))
            .unwrap();

        let f_write = virtio_bindings::bindings::virtio_ring::VRING_DESC_F_WRITE as u16;

        build_request(
            &mem,
            &mut queue,
            &[
                (header_addr, 16, 0),
                (data_addr, 512, f_write),
                (status_addr, 1, f_write),
            ],
        );

        let _ = blk.queue_notify(0, &mut queue, &mem);
        assert_eq!(VIRTIO_BLK_S_IOERR, read_status(&mem, status_addr));
    }

    #[test]
    fn test_unsupported_request_type() {
        let disk_path = test_file_path();
        let _disk = make_disk(&disk_path);
        let (mem, mut queue, mut blk) = setup(&disk_path);

        let header_addr = 0x4000u64;
        let status_addr = 0x5000u64;

        let mut header_bytes = [0u8; 16];
        header_bytes[0..4].copy_from_slice(&99u32.to_le_bytes());
        mem.write_slice(&header_bytes, GuestAddress(header_addr))
            .unwrap();

        let f_write = virtio_bindings::bindings::virtio_ring::VRING_DESC_F_WRITE as u16;

        build_request(
            &mem,
            &mut queue,
            &[(header_addr, 16, 0), (status_addr, 1, f_write)],
        );

        let _ = blk.queue_notify(0, &mut queue, &mem);
        assert_eq!(VIRTIO_BLK_S_UNSUPP, read_status(&mem, status_addr));
    }

    #[test]
    fn test_reset() {
        let disk_path = test_file_path();
        let _disk = make_disk(&disk_path);
        let (_mem, _queue, mut blk) = setup(&disk_path);
        blk.reset();
    }

    #[test]
    fn test_write_config_noop() {
        let disk_path = test_file_path();
        let _disk = make_disk(&disk_path);
        let (_mem, _queue, mut blk) = setup(&disk_path);
        blk.write_config(0, &[1, 2, 3, 4]);
        let mut buf = [0u8; 8];
        blk.read_config(0, &mut buf);
        assert_eq!(2048u64.to_le_bytes(), buf);
    }
}

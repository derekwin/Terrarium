//! virtio-mem 设备（M1 Task 3）。
//!
//! device_id=14，单队列（size 128），支持 guest 物理内存在线伸缩。
//! 热插拔内存区位于 [4GiB, 4GiB+hotplug_max)，使用独立 memslot 与
//! 宿主匿名 mmap 映射。plug 时 MADV_POPULATE_WRITE，unplug 时 MADV_DONTNEED。
//!
//! 配置空间（offset 相对 0x100）：
//!   0x00: u64 block_size（2MiB）
//!   0x08: u64 addr（热插拔区 guest 物理基址 = 4GiB）
//!   0x10: u64 region_size（热插拔区总大小）
//!   0x18: u64 usable_region_size（当前可用大小）
//!   0x20: u64 plugged_size（guest 已插拔大小）
//!   0x28: u64 requested_size（host 期望大小，resize 命令修改此项）

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_CONFIG_CHANGE, ISR_USED_BUFFER};

/// virtio-mem 设备号。
const VIRTIO_ID_MEM: u32 = 14;

/// 内存块大小：2MiB（与 Linux THP 页对齐，减少碎片）。
const BLOCK_SIZE: u64 = 2 * 1024 * 1024;

/// virtio-mem 请求类型。
const VIRTIO_MEM_REQ_PLUG: u16 = 0;
const VIRTIO_MEM_REQ_UNPLUG: u16 = 1;
const VIRTIO_MEM_REQ_UNPLUG_ALL: u16 = 2;
const VIRTIO_MEM_REQ_STATE: u16 = 3;

/// 请求/响应状态。
const VIRTIO_MEM_RESP_ACK: u16 = 0;
const VIRTIO_MEM_RESP_ERROR: u16 = 2;

/// 热插拔内存区在 guest 物理地址空间中的基址。
pub const MEM_HOTPLUG_BASE: u64 = 4 * 1024 * 1024 * 1024; // 4GiB

/// virtio-mem 设备，线程安全地管理热插拔内存。
///
/// 使用 `Arc<AtomicU64>` 共享 plugged_size / requested_size，以便
/// API 线程（resize_mem）和 vCPU 线程（请求队列处理）可以安全访问。
pub struct Mem {
    /// 热插拔区总大小（字节）。
    region_size: u64,
    /// 当前已 plug 的大小（字节），跨线程共享。
    plugged_size: Arc<AtomicU64>,
    /// host 期望的大小（字节），resize_mem API 写入此项。
    requested_size: Arc<AtomicU64>,
    /// 是否有待处理的 config change 中断。
    config_changed: Arc<AtomicBool>,
    /// 宿主侧热插拔内存映射（整个热插拔区）。
    hotplug_mem: GuestMemoryMmap,
}

impl Mem {
    /// 创建 virtio-mem 设备。
    ///
    /// `hotplug_mib` 是热插拔区上限（MiB）。
    pub fn new(hotplug_mib: usize, hotplug_mem: GuestMemoryMmap) -> Self {
        let region_size = (hotplug_mib as u64) << 20;
        Mem {
            region_size,
            plugged_size: Arc::new(AtomicU64::new(0)),
            requested_size: Arc::new(AtomicU64::new(0)),
            config_changed: Arc::new(AtomicBool::new(false)),
            hotplug_mem,
        }
    }

    /// 获取 requested_size 的 Arc clone（供 API handler 直接写入）。
    pub fn requested_size_arc(&self) -> Arc<AtomicU64> {
        self.requested_size.clone()
    }

    /// 获取 config_changed 的 Arc clone（供 API handler 写入以触发中断）。
    pub fn config_changed_arc(&self) -> Arc<AtomicBool> {
        self.config_changed.clone()
    }

    /// 处理 resize 命令：更新 requested_size，标记 config change。
    pub fn resize(&mut self, new_size_bytes: u64) {
        let clamped = new_size_bytes.min(self.region_size);
        // 对齐到 block_size 边界。
        let aligned = (clamped / BLOCK_SIZE) * BLOCK_SIZE;
        self.requested_size.store(aligned, Ordering::SeqCst);
        self.config_changed.store(true, Ordering::SeqCst);
    }

    /// 消费 config change 信号（供 `pending_interrupts` 使用）。
    fn consume_config_changed(&self) -> bool {
        self.config_changed.swap(false, Ordering::SeqCst)
    }

    fn process_one(&self, chain: DescriptorChain<&GuestMemoryMmap>) -> io::Result<usize> {
        let mem = chain.memory().clone();
        let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();

        if descs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "空描述符链"));
        }

        // 请求描述符：24 字节（type u16 + padding[2] + addr u64 + nb_blocks u16 + padding[2]）
        // 响应描述符：状态 + 可选 state 数据
        let req_desc = &descs[0];
        let req_len = req_desc.len() as usize;
        if req_len < 24 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "请求头太短"));
        }

        let req_addr = req_desc.addr();
        let mut req_buf = [0u8; 24];
        mem.read_slice(&mut req_buf, req_addr)
            .map_err(io::Error::other)?;

        let req_type = u16::from_le_bytes(req_buf[0..2].try_into().unwrap());
        let addr = u64::from_le_bytes(req_buf[4..12].try_into().unwrap());
        let nb_blocks = u16::from_le_bytes(req_buf[12..14].try_into().unwrap());

        // 找到响应描述符（可写描述符）。
        let resp_idx = descs
            .iter()
            .rposition(|d| d.is_write_only())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "缺少响应描述符"))?;
        let resp_addr = descs[resp_idx].addr();

        let result = match req_type {
            VIRTIO_MEM_REQ_PLUG => self.handle_plug(addr, nb_blocks),
            VIRTIO_MEM_REQ_UNPLUG => self.handle_unplug(addr, nb_blocks),
            VIRTIO_MEM_REQ_UNPLUG_ALL => self.handle_unplug_all(),
            VIRTIO_MEM_REQ_STATE => self.handle_state(&mem, resp_addr),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "未知请求类型")),
        };

        // 写入响应状态。
        let resp_state: u16 = if result.is_ok() {
            VIRTIO_MEM_RESP_ACK
        } else {
            VIRTIO_MEM_RESP_ERROR
        };
        mem.write_obj(resp_state.to_le_bytes(), resp_addr)
            .map_err(io::Error::other)?;

        Ok(2)
    }

    fn handle_plug(&self, addr: u64, nb_blocks: u16) -> io::Result<()> {
        let offset = addr
            .checked_sub(MEM_HOTPLUG_BASE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "地址不在热插拔区"))?;
        let size = nb_blocks as u64 * BLOCK_SIZE;
        if offset + size > self.region_size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "超出热插拔区"));
        }

        // 预填充物理页（MADV_POPULATE_WRITE）。
        let host_addr = self
            .hotplug_mem
            .get_host_address(GuestAddress(offset))
            .map_err(io::Error::other)?;
        self.madvise_populate(host_addr, size as usize)?;

        self.plugged_size.fetch_add(size, Ordering::SeqCst);
        Ok(())
    }

    fn handle_unplug(&self, addr: u64, nb_blocks: u16) -> io::Result<()> {
        let offset = addr
            .checked_sub(MEM_HOTPLUG_BASE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "地址不在热插拔区"))?;
        let size = nb_blocks as u64 * BLOCK_SIZE;
        if offset + size > self.region_size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "超出热插拔区"));
        }

        // 释放物理页（MADV_DONTNEED）。
        let host_addr = self
            .hotplug_mem
            .get_host_address(GuestAddress(offset))
            .map_err(io::Error::other)?;
        self.madvise_dontneed(host_addr, size as usize)?;

        let current = self.plugged_size.load(Ordering::SeqCst);
        let new_size = current.saturating_sub(size);
        self.plugged_size.store(new_size, Ordering::SeqCst);
        Ok(())
    }

    fn handle_unplug_all(&self) -> io::Result<()> {
        let current = self.plugged_size.load(Ordering::SeqCst);
        if current > 0 {
            let host_addr = self
                .hotplug_mem
                .get_host_address(GuestAddress(0))
                .map_err(io::Error::other)?;
            self.madvise_dontneed(host_addr, current as usize)?;
            self.plugged_size.store(0, Ordering::SeqCst);
        }
        Ok(())
    }

    fn handle_state(&self, mem: &GuestMemoryMmap, resp_addr: GuestAddress) -> io::Result<()> {
        // STATE 响应格式：
        //   u16 type = STATE (从请求复制)
        //   u16 state = ACK
        //   u64 plugged_size
        //   u64 usable_region_size
        //   ... 更多字段（简化实现只写这三个）
        let plugged = self.plugged_size.load(Ordering::SeqCst);
        let usable = self.requested_size.load(Ordering::SeqCst);

        mem.write_obj(plugged.to_le_bytes(), resp_addr)
            .map_err(io::Error::other)?;
        mem.write_obj(usable.to_le_bytes(), resp_addr.unchecked_add(8))
            .map_err(io::Error::other)?;
        Ok(())
    }

    #[allow(unsafe_code)]
    fn madvise_populate(&self, ptr: *mut u8, len: usize) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        // SAFETY: ptr 指向 hotplug_mem 拥有的匿名 mmap 区域。
        // MADV_POPULATE_WRITE 预填充页表项，减少 guest 首次访问时的缺页开销。
        #[cfg(target_os = "linux")]
        unsafe {
            let ret = libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_POPULATE_WRITE);
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (ptr, len);
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    fn madvise_dontneed(&self, ptr: *mut u8, len: usize) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        // SAFETY: ptr 指向 hotplug_mem 拥有的匿名 mmap 区域。
        // MADV_DONTNEED 释放物理页，guest 再次访问时重新缺页（返回零页）。
        #[cfg(target_os = "linux")]
        unsafe {
            let ret = libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_DONTNEED);
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (ptr, len);
        }
        Ok(())
    }
}

impl VirtioDevice for Mem {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_MEM
    }

    fn features(&self) -> u64 {
        0 // 无额外特性位
    }

    fn queue_count(&self) -> usize {
        1
    }

    fn queue_max_size(&self) -> u16 {
        128
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // 配置空间布局：
        // 0x00: block_size (u64, 2MiB)
        // 0x08: addr (u64, 4GiB)
        // 0x10: region_size (u64)
        // 0x18: usable_region_size (u64) = plugged_size
        // 0x20: plugged_size (u64)
        // 0x28: requested_size (u64)
        let plugged = self.plugged_size.load(Ordering::SeqCst);
        let requested = self.requested_size.load(Ordering::SeqCst);
        let field_bytes: [&[u8]; 6] = [
            &BLOCK_SIZE.to_le_bytes(),
            &MEM_HOTPLUG_BASE.to_le_bytes(),
            &self.region_size.to_le_bytes(),
            &plugged.to_le_bytes(),
            &plugged.to_le_bytes(),
            &requested.to_le_bytes(),
        ];
        let field_offsets: [u64; 6] = [0x00, 0x08, 0x10, 0x18, 0x20, 0x28];

        let start = offset as usize;
        let end = start + data.len();
        let mut buf = vec![0u8; end];
        for (i, bytes) in field_bytes.iter().enumerate() {
            let base = field_offsets[i] as usize;
            if base + 8 <= buf.len() {
                buf[base..base + 8].copy_from_slice(bytes);
            }
        }
        let copy_end = end.min(buf.len());
        data[..copy_end - start].copy_from_slice(&buf[start..copy_end]);
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // 配置空间只读。
    }

    fn queue_notify(
        &mut self,
        _queue_index: usize,
        queue: &mut Queue,
        mem: &GuestMemoryMmap,
    ) -> u32 {
        let mut used_any = false;

        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let result = self.process_one(chain);
            let len = result.unwrap_or(0);
            let _ = queue.add_used(mem, head, len as u32 + 2);
            used_any = true;
        }

        if used_any {
            ISR_USED_BUFFER
        } else {
            0
        }
    }

    fn pending_interrupts(&self) -> u32 {
        if self.consume_config_changed() {
            ISR_CONFIG_CHANGE
        } else {
            0
        }
    }

    fn reset(&mut self) {
        self.plugged_size.store(0, Ordering::SeqCst);
        self.requested_size.store(0, Ordering::SeqCst);
        self.config_changed.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_mem() -> Mem {
        let hotplug_mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 64 << 20)]).unwrap();
        Mem::new(64, hotplug_mem)
    }

    #[test]
    fn test_device_identity() {
        let mem = test_mem();
        assert_eq!(14, mem.device_id());
        assert_eq!(0, mem.features());
        assert_eq!(1, mem.queue_count());
        assert_eq!(128, mem.queue_max_size());
    }

    #[test]
    fn test_config_space() {
        let mem = test_mem();
        let mut buf = [0xffu8; 8];
        // block_size at offset 0
        mem.read_config(0, &mut buf);
        assert_eq!(BLOCK_SIZE.to_le_bytes(), buf);
        // addr at offset 8
        mem.read_config(8, &mut buf);
        assert_eq!(MEM_HOTPLUG_BASE.to_le_bytes(), buf);
    }

    #[test]
    fn test_resize_triggers_config_change() {
        let mut mem = test_mem();
        assert_eq!(0, mem.pending_interrupts());
        mem.resize(32 << 20); // 32MiB
        assert_eq!(ISR_CONFIG_CHANGE, mem.pending_interrupts());
        // 消费后不再报告
        assert_eq!(0, mem.pending_interrupts());
    }

    #[test]
    fn test_resize_clamps_to_region_size() {
        let mut mem = test_mem();
        // region_size = 64MiB, resize to 128MiB → clamped to 64MiB
        mem.resize(128 << 20);
        let mut buf = [0u8; 8];
        mem.read_config(0x28, &mut buf);
        assert_eq!((64u64 << 20).to_le_bytes(), buf);
    }

    #[test]
    fn test_resize_aligns_to_block_size() {
        let mut mem = test_mem();
        mem.resize(10 * 1024 * 1024 + 1000); // ~10MiB + extra
        let mut buf = [0u8; 8];
        mem.read_config(0x28, &mut buf);
        let val = u64::from_le_bytes(buf);
        assert_eq!(val % BLOCK_SIZE, 0);
    }

    #[test]
    fn test_plug_unplug() {
        let mem = test_mem();
        // plug 2MiB at offset 0
        assert!(mem.handle_plug(MEM_HOTPLUG_BASE, 1).is_ok());
        // unplug 2MiB at offset 0
        assert!(mem.handle_unplug(MEM_HOTPLUG_BASE, 1).is_ok());
    }

    #[test]
    fn test_plug_out_of_bounds() {
        let mem = test_mem();
        // 64MiB region = 32 blocks of 2MiB each
        // Try to plug 33 blocks → should fail
        assert!(mem.handle_plug(MEM_HOTPLUG_BASE, 33).is_err());
    }

    #[test]
    fn test_plug_wrong_address() {
        let mem = test_mem();
        // Address not in hotplug region
        assert!(mem.handle_plug(0, 1).is_err());
    }

    #[test]
    fn test_reset_clears_state() {
        let mut mem = test_mem();
        mem.resize(32 << 20);
        let _ = mem.handle_plug(MEM_HOTPLUG_BASE, 1);
        mem.reset();
        assert_eq!(0, mem.plugged_size.load(Ordering::SeqCst));
        assert_eq!(0, mem.requested_size.load(Ordering::SeqCst));
        assert!(!mem.config_changed.load(Ordering::SeqCst));
    }
}

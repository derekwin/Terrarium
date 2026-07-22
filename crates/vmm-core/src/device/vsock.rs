//! virtio-vsock 设备（M1 Task 5）。
//!
//! device_id=13，3 队列（rx/tx/event），features=VIRTIO_F_VERSION_1（传输层自动附加）。
//! guest cid=3（host=2）。guest 连接端口映射到宿主 `/tmp/vsock.{port}` Unix socket。
//! 数据包格式按 virtio-vsock spec：44 字节头 + payload。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{Address, Bytes, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_USED_BUFFER};

const VIRTIO_ID_VSOCK: u32 = 13;

const GUEST_CID: u64 = 3;
const HOST_CID: u64 = 2;

const VSOCK_OP_REQUEST: u16 = 1;
const VSOCK_OP_RST: u16 = 3;
const VSOCK_OP_SHUTDOWN: u16 = 4;
const VSOCK_OP_RW: u16 = 5;

const PKT_HEADER_SIZE: usize = 44;

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

/// 接收缓冲区上限。
const RX_BUF_SIZE: usize = 4096;

/// 活跃连接：一个 guest 端口对应一个宿主 Unix socket。
struct Connection {
    stream: UnixStream,
}

pub struct Vsock {
    connections: HashMap<u32, Connection>,
}

impl Default for Vsock {
    fn default() -> Self {
        Self::new()
    }
}

impl Vsock {
    pub fn new() -> Self {
        Vsock {
            connections: HashMap::new(),
        }
    }

    fn process_rx(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        let mut used_any = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let written = self.process_rx_chain(chain, mem);
            let _ = queue.add_used(mem, head, written);
            used_any = true;
        }
        if used_any {
            ISR_USED_BUFFER
        } else {
            0
        }
    }

    fn process_tx(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        let mut used_any = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            self.process_tx_chain(chain, mem);
            let _ = queue.add_used(mem, head, 0);
            used_any = true;
        }
        if used_any {
            ISR_USED_BUFFER
        } else {
            0
        }
    }

    /// RX：将宿主 socket 数据写入 guest 的 RX buffer。
    fn process_rx_chain(
        &mut self,
        chain: DescriptorChain<&GuestMemoryMmap>,
        mem: &GuestMemoryMmap,
    ) -> u32 {
        let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
        if descs.is_empty() {
            return 0;
        }

        let buf_addr = descs[0].addr();
        let buf_len = descs[0].len().min(RX_BUF_SIZE as u32) as usize;

        // 遍历所有连接，尝试读取数据并发送给 guest。
        let ports: Vec<u32> = self.connections.keys().copied().collect();
        for port in ports {
            if let Some(conn) = self.connections.get_mut(&port) {
                let mut data = vec![0u8; buf_len.saturating_sub(PKT_HEADER_SIZE)];
                match conn.stream.read(&mut data) {
                    Ok(0) => {
                        // 对端关闭：发送 SHUTDOWN 通知。
                        self.write_pkt(mem, buf_addr, port, HOST_CID, VSOCK_OP_SHUTDOWN, &[]);
                        return (PKT_HEADER_SIZE) as u32;
                    }
                    Ok(n) => {
                        self.write_pkt(mem, buf_addr, port, HOST_CID, VSOCK_OP_RW, &data[..n]);
                        return (PKT_HEADER_SIZE + n) as u32;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(_) => {
                        self.connections.remove(&port);
                        self.write_pkt(mem, buf_addr, port, HOST_CID, VSOCK_OP_RST, &[]);
                        return (PKT_HEADER_SIZE) as u32;
                    }
                }
            }
        }
        0
    }

    fn process_tx_chain(
        &mut self,
        chain: DescriptorChain<&GuestMemoryMmap>,
        mem: &GuestMemoryMmap,
    ) {
        let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
        if descs.is_empty() {
            return;
        }
        let header_addr = descs[0].addr();
        if (descs[0].len() as usize) < PKT_HEADER_SIZE {
            return;
        }
        let mut hdr = [0u8; PKT_HEADER_SIZE];
        if mem.read_slice(&mut hdr, header_addr).is_err() {
            return;
        }

        let src_port = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
        let dst_port = u32::from_le_bytes(hdr[20..24].try_into().unwrap());
        let len = u32::from_le_bytes(hdr[24..28].try_into().unwrap()) as usize;
        let op = u16::from_le_bytes(hdr[30..32].try_into().unwrap());

        let payload_offset = PKT_HEADER_SIZE;
        let payload_end = payload_offset + len.min(descs[0].len() as usize - PKT_HEADER_SIZE);

        match op {
            VSOCK_OP_REQUEST => {
                // 连接到宿主 Unix socket。
                let path = format!("/tmp/vsock.{}", dst_port);
                if let Ok(stream) = UnixStream::connect(&path) {
                    let _ = stream.set_nonblocking(true);
                    self.connections.insert(src_port, Connection { stream });
                }
            }
            VSOCK_OP_RW => {
                if let Some(conn) = self.connections.get_mut(&src_port) {
                    if payload_end > payload_offset {
                        let mut data = vec![0u8; payload_end - payload_offset];
                        if mem
                            .read_slice(&mut data, header_addr.unchecked_add(payload_offset as u64))
                            .is_ok()
                        {
                            let _ = conn.stream.write_all(&data);
                        }
                    }
                }
            }
            VSOCK_OP_SHUTDOWN | VSOCK_OP_RST => {
                self.connections.remove(&src_port);
            }
            _ => {}
        }
    }

    /// 向 guest RX buffer 写入一个数据包。
    fn write_pkt(
        &self,
        mem: &GuestMemoryMmap,
        addr: vm_memory::GuestAddress,
        src_port: u32,
        dst_cid: u64,
        op: u16,
        payload: &[u8],
    ) {
        let mut hdr = [0u8; PKT_HEADER_SIZE];
        hdr[0..8].copy_from_slice(&GUEST_CID.to_le_bytes());
        hdr[8..16].copy_from_slice(&dst_cid.to_le_bytes());
        hdr[16..20].copy_from_slice(&src_port.to_le_bytes());
        hdr[28..30].copy_from_slice(&1u16.to_le_bytes()); // type = stream
        hdr[30..32].copy_from_slice(&op.to_le_bytes());
        hdr[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());

        let _ = mem.write_slice(&hdr, addr);
        if !payload.is_empty() {
            let _ = mem.write_slice(payload, addr.unchecked_add(PKT_HEADER_SIZE as u64));
        }
    }
}

impl VirtioDevice for Vsock {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_VSOCK
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_count(&self) -> usize {
        3
    }

    fn queue_max_size(&self) -> u16 {
        256
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        if offset < 8 {
            let cid_bytes = GUEST_CID.to_le_bytes();
            let start = offset as usize;
            let end = (start + data.len()).min(8);
            data[..end - start].copy_from_slice(&cid_bytes[start..end]);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn queue_notify(
        &mut self,
        queue_index: usize,
        queue: &mut Queue,
        mem: &GuestMemoryMmap,
    ) -> u32 {
        match queue_index {
            RX_QUEUE => self.process_rx(queue, mem),
            TX_QUEUE => self.process_tx(queue, mem),
            _ => 0,
        }
    }

    fn reset(&mut self) {
        self.connections.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_identity() {
        let vsock = Vsock::new();
        assert_eq!(13, vsock.device_id());
        assert_eq!(0, vsock.features());
        assert_eq!(3, vsock.queue_count());
        assert_eq!(256, vsock.queue_max_size());
    }

    #[test]
    fn test_config_cid() {
        let vsock = Vsock::new();
        let mut buf = [0xffu8; 8];
        vsock.read_config(0, &mut buf);
        assert_eq!(GUEST_CID.to_le_bytes(), buf);
    }

    #[test]
    fn test_write_config_noop() {
        let mut vsock = Vsock::new();
        vsock.write_config(0, &[1, 2, 3, 4]);
        let mut buf = [0u8; 8];
        vsock.read_config(0, &mut buf);
        assert_eq!(GUEST_CID.to_le_bytes(), buf);
    }

    #[test]
    fn test_reset_clears_connections() {
        let mut vsock = Vsock::new();
        vsock.reset();
        assert!(vsock.connections.is_empty());
    }
}

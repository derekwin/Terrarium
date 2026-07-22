//! virtio-vsock 设备（M1 Task 5）。
//!
//! device_id=13，3 队列（rx/tx/event），features=VIRTIO_F_VERSION_1（传输层自动附加）。
//! guest cid=3（host=2），数据包格式按 virtio-vsock spec：
//! 44 字节头 + payload。
//!
//! M1 范围：设备注册与数据包格式解析；宿主 Unix socket 转发留待后续。

use std::collections::HashMap;
use std::os::unix::net::UnixStream;

use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_USED_BUFFER};

const VIRTIO_ID_VSOCK: u32 = 13;

/// Guest CID（Context Identifier，vsock 地址空间中的 VM 标识）。
const GUEST_CID: u64 = 3;

/// virtio-vsock 数据包类型。
const VSOCK_TYPE_STREAM: u16 = 1;

/// virtio-vsock 操作码。
const VSOCK_OP_REQUEST: u16 = 1;
const VSOCK_OP_RESPONSE: u16 = 2;
const VSOCK_OP_RST: u16 = 3;
const VSOCK_OP_SHUTDOWN: u16 = 4;
const VSOCK_OP_RW: u16 = 5;
const VSOCK_OP_CREDIT_UPDATE: u16 = 6;
const VSOCK_OP_CREDIT_REQUEST: u16 = 7;

/// 数据包子节头大小（44 bytes，不含 payload）。
const PKT_HEADER_SIZE: usize = 44;

/// 队列索引。
const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;
const _EVENT_QUEUE: usize = 2;

/// virtio-vsock 数据包（44 字节头）。
#[repr(C, packed)]
struct VsockPacket {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    type_: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

/// 连接状态。
#[derive(Debug)]
struct Connection {
    /// 宿主侧 Unix socket 流。
    stream: UnixStream,
    /// 对端端口。
    _peer_port: u32,
}

/// virtio-vsock 设备。
pub struct Vsock {
    /// 活跃连接表：local_port → Connection。
    connections: HashMap<u32, Connection>,
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
            self.process_rx_chain(chain, mem);
            let _ = queue.add_used(mem, head, 0);
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

    fn process_rx_chain(
        &mut self,
        chain: DescriptorChain<&GuestMemoryMmap>,
        _mem: &GuestMemoryMmap,
    ) {
        let _ = chain;
        // RX 队列：设备 → guest 的数据。
        // M1 仅注册设备，不实现宿主 socket 转发。
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

        // 读取数据包头（第一个描述符的前 44 字节）。
        let header_addr = descs[0].addr();
        let header_len = descs[0].len() as usize;
        if header_len < PKT_HEADER_SIZE {
            return;
        }

        let mut header_buf = [0u8; PKT_HEADER_SIZE];
        if mem.read_slice(&mut header_buf, header_addr).is_err() {
            return;
        }

        let pkt = VsockPacket {
            src_cid: u64::from_le_bytes(header_buf[0..8].try_into().unwrap()),
            dst_cid: u64::from_le_bytes(header_buf[8..16].try_into().unwrap()),
            src_port: u32::from_le_bytes(header_buf[16..20].try_into().unwrap()),
            dst_port: u32::from_le_bytes(header_buf[20..24].try_into().unwrap()),
            len: u32::from_le_bytes(header_buf[24..28].try_into().unwrap()),
            type_: u16::from_le_bytes(header_buf[28..30].try_into().unwrap()),
            op: u16::from_le_bytes(header_buf[30..32].try_into().unwrap()),
            flags: u32::from_le_bytes(header_buf[32..36].try_into().unwrap()),
            buf_alloc: u32::from_le_bytes(header_buf[36..40].try_into().unwrap()),
            fwd_cnt: u32::from_le_bytes(header_buf[40..44].try_into().unwrap()),
        };

        self.handle_packet(&pkt, &descs, mem);
    }

    fn handle_packet(
        &mut self,
        pkt: &VsockPacket,
        _descs: &[virtio_queue::desc::split::Descriptor],
        _mem: &GuestMemoryMmap,
    ) {
        // 仅处理发给 guest 的 packet（dst_cid == GUEST_CID）。
        if pkt.dst_cid != GUEST_CID {
            return;
        }

        match pkt.op {
            VSOCK_OP_REQUEST => {
                // 新的连接请求：发送 RST（M1 不做真实 socket 转发）。
                self.send_reset(pkt);
            }
            VSOCK_OP_SHUTDOWN | VSOCK_OP_RST => {
                let port = { pkt.src_port };
                self.connections.remove(&port);
            }
            _ => {
                // RW / CREDIT 等操作：M1 仅做协议解析。
            }
        }
    }

    fn send_reset(&self, pkt: &VsockPacket) {
        // 向 guest 发送 RST 包（通过 RX 队列）。
        // M1 仅记录日志；实际发送需写 RX 队列的 used ring。
        let _ = pkt;
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
        // config space: guest_cid (u64) at offset 0
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

        // Partial read
        let mut buf2 = [0u8; 4];
        vsock.read_config(4, &mut buf2);
        assert_eq!(&GUEST_CID.to_le_bytes()[4..], &buf2[..]);
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
    fn test_reset() {
        let mut vsock = Vsock::new();
        vsock.reset();
        assert_eq!(0, vsock.connections.len());
    }
}

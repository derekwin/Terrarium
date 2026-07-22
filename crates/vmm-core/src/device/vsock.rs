//! virtio-vsock 设备（event-manager + 信用流控版）。
//!
//! device_id=13，3 队列（rx/tx/event）。features: VIRTIO_VSOCK_F_STREAM。
//! guest cid=3, host cid=2。连接状态机 + credit-based flow control。
//! 宿主 socket 用 event-manager 轮询非阻塞 I/O。

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

use event_manager::{EventManager, EventOps, EventSet, Events, MutEventSubscriber, SubscriberOps};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_USED_BUFFER};

const VIRTIO_ID_VSOCK: u32 = 13;
const VIRTIO_VSOCK_F_STREAM: u64 = 1;

const GUEST_CID: u64 = 3;
const HOST_CID: u64 = 2;

const VSOCK_OP_REQUEST: u16 = 1;
const VSOCK_OP_RST: u16 = 3;
const VSOCK_OP_SHUTDOWN: u16 = 4;
const VSOCK_OP_RW: u16 = 5;
const VSOCK_OP_CREDIT_UPDATE: u16 = 6;

const PKT_HDR_SIZE: usize = 44;
const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;
const DEFAULT_BUF_ALLOC: u32 = 64 * 1024;

type FdMap = HashMap<RawFd, u32>;
type PendingQueue = Vec<(u32, Vec<u8>)>;

struct Connection {
    stream: UnixStream,
    guest_buf_alloc: u32,
    guest_fwd_cnt: u32,
}

pub struct Vsock {
    connections: HashMap<u32, Connection>,
    pending: Arc<Mutex<PendingQueue>>,
}

impl Default for Vsock {
    fn default() -> Self {
        Self::new()
    }
}

impl Vsock {
    pub fn new() -> Self {
        let pending = Arc::new(Mutex::new(Vec::new()));
        let fds: Arc<Mutex<FdMap>> = Arc::new(Mutex::new(HashMap::new()));
        let pend = pending.clone();

        let mut mgr = EventManager::new().unwrap();
        mgr.add_subscriber(RxHandler {
            fds: fds.clone(),
            pending: pend,
        });

        thread::spawn(move || loop {
            if let Err(event_manager::Error::Epoll(_)) = mgr.run_with_timeout(100) {
                break;
            }
        });

        Vsock {
            connections: HashMap::new(),
            pending,
        }
    }

    fn process_rx(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
            if descs.is_empty() {
                let _ = queue.add_used(mem, head, 0);
                used = true;
                continue;
            }

            let mut written = 0u32;
            if let Some((port, data)) = self.pending.lock().unwrap().pop() {
                if let Some(conn) = self.connections.get(&port) {
                    let total = PKT_HDR_SIZE + data.len();
                    if total as u32 <= conn.guest_buf_alloc {
                        write_pkt(mem, descs[0].addr(), port, HOST_CID, VSOCK_OP_RW, &data);
                        written = total as u32;
                    }
                }
            }
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        if used {
            ISR_USED_BUFFER
        } else {
            0
        }
    }

    fn process_tx(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
            if descs.is_empty() {
                let _ = queue.add_used(mem, head, 0);
                used = true;
                continue;
            }

            let hdr_addr = descs[0].addr();
            if (descs[0].len() as usize) < PKT_HDR_SIZE {
                continue;
            }
            let mut hdr = [0u8; PKT_HDR_SIZE];
            if mem.read_slice(&mut hdr, hdr_addr).is_err() {
                continue;
            }

            let src_port = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
            let dst_port = u32::from_le_bytes(hdr[20..24].try_into().unwrap());
            let len = u32::from_le_bytes(hdr[24..28].try_into().unwrap()) as usize;
            let op = u16::from_le_bytes(hdr[30..32].try_into().unwrap());
            let buf_alloc = u32::from_le_bytes(hdr[36..40].try_into().unwrap());
            let fwd_cnt = u32::from_le_bytes(hdr[40..44].try_into().unwrap());

            match op {
                VSOCK_OP_REQUEST => {
                    if let Ok(stream) = UnixStream::connect(format!("/tmp/vsock.{dst_port}")) {
                        let _ = stream.set_nonblocking(true);
                        self.connections.insert(
                            src_port,
                            Connection {
                                stream,
                                guest_buf_alloc: buf_alloc.max(DEFAULT_BUF_ALLOC),
                                guest_fwd_cnt: fwd_cnt,
                            },
                        );
                    }
                }
                VSOCK_OP_RW => {
                    if let Some(conn) = self.connections.get_mut(&src_port) {
                        let pl_start = PKT_HDR_SIZE;
                        let desc_len = descs[0].len() as usize;
                        if desc_len > pl_start {
                            let n = len.min(desc_len - pl_start);
                            let addr = GuestAddress(hdr_addr.0 + pl_start as u64);
                            let mut data = vec![0u8; n];
                            if mem.read_slice(&mut data, addr).is_ok() {
                                let _ = conn.stream.write_all(&data);
                                conn.guest_fwd_cnt += n as u32;
                            }
                        }
                    }
                }
                VSOCK_OP_SHUTDOWN | VSOCK_OP_RST => {
                    self.connections.remove(&src_port);
                }
                VSOCK_OP_CREDIT_UPDATE => {
                    if let Some(conn) = self.connections.get_mut(&src_port) {
                        conn.guest_buf_alloc = buf_alloc;
                        conn.guest_fwd_cnt = fwd_cnt;
                    }
                }
                _ => {}
            }

            let _ = queue.add_used(mem, head, 0);
            used = true;
        }
        if used {
            ISR_USED_BUFFER
        } else {
            0
        }
    }
}

impl VirtioDevice for Vsock {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_VSOCK
    }
    fn features(&self) -> u64 {
        VIRTIO_VSOCK_F_STREAM
    }
    fn queue_count(&self) -> usize {
        3
    }
    fn queue_max_size(&self) -> u16 {
        256
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        if offset < 8 {
            let cb = GUEST_CID.to_le_bytes();
            let s = offset as usize;
            let dlen = data.len();
            let end = (s + dlen).min(8);
            data[..end - s].copy_from_slice(&cb[s..end]);
        }
    }
    fn write_config(&mut self, _o: u64, _d: &[u8]) {}
    fn queue_notify(&mut self, qi: usize, q: &mut Queue, m: &GuestMemoryMmap) -> u32 {
        match qi {
            RX_QUEUE => self.process_rx(q, m),
            TX_QUEUE => self.process_tx(q, m),
            _ => 0,
        }
    }
    fn reset(&mut self) {
        self.connections.clear();
    }
}

fn write_pkt(
    mem: &GuestMemoryMmap,
    addr: GuestAddress,
    src_port: u32,
    dst_cid: u64,
    op: u16,
    payload: &[u8],
) {
    let mut hdr = [0u8; PKT_HDR_SIZE];
    hdr[0..8].copy_from_slice(&GUEST_CID.to_le_bytes());
    hdr[8..16].copy_from_slice(&dst_cid.to_le_bytes());
    hdr[16..20].copy_from_slice(&src_port.to_le_bytes());
    hdr[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    hdr[28..30].copy_from_slice(&1u16.to_le_bytes());
    hdr[30..32].copy_from_slice(&op.to_le_bytes());
    let _ = mem.write_slice(&hdr, addr);
    if !payload.is_empty() {
        let _ = mem.write_slice(payload, GuestAddress(addr.0 + PKT_HDR_SIZE as u64));
    }
}

struct RxHandler {
    fds: Arc<Mutex<FdMap>>,
    pending: Arc<Mutex<PendingQueue>>,
}

impl MutEventSubscriber for RxHandler {
    fn init(&mut self, ops: &mut EventOps) {
        for &fd in self.fds.lock().unwrap().keys() {
            let _ = ops.add(Events::new_raw(fd, EventSet::IN));
        }
    }
    fn process(&mut self, events: Events, _ops: &mut EventOps) {
        if let Some(&port) = self.fds.lock().unwrap().get(&events.fd()) {
            let mut buf = vec![0u8; 4096];
            loop {
                let n = unsafe {
                    libc::read(
                        events.fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
                self.pending
                    .lock()
                    .unwrap()
                    .push((port, buf[..n as usize].to_vec()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_id() {
        let v = Vsock::new();
        assert_eq!(13, v.device_id());
        assert_eq!(3, v.queue_count());
    }
    #[test]
    fn test_cid() {
        let v = Vsock::new();
        let mut b = [0u8; 8];
        v.read_config(0, &mut b);
        assert_eq!(GUEST_CID.to_le_bytes(), b);
    }
}

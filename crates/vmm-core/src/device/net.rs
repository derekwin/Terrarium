//! virtio-net 设备（event-manager 版）。
//!
//! RX: event-manager 线程订阅 TAP fd 可读事件 → 帧队列。
//! TX: queue_notify 同步 write 到 TAP fd。
//! features: VIRTIO_NET_F_MAC | VIRTIO_NET_F_MRG_RXBUF | VIRTIO_NET_F_STATUS。

#![allow(unsafe_code)]

use std::cmp;
use std::io;
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};
use std::thread;

use event_manager::{EventManager, EventOps, EventSet, Events, MutEventSubscriber, SubscriberOps};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_USED_BUFFER};

const VIRTIO_ID_NET: u32 = 1;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_S_LINK_UP: u16 = 1;

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

const NET_HDR_SIZE: usize = 12;
const MAX_FRAME: usize = 65562;
const FRAME_CAP: usize = 64;

const MAC_ADDR: [u8; 6] = [0x02, 0x54, 0x45, 0x52, 0x52, 0x41];
const MTU: u16 = 1500;

/// TAP 收包处理器（event-manager subscriber）。
struct NetRxHandler {
    read_fd: RawFd,
    rx_frames: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl NetRxHandler {
    fn read_and_enqueue(&self) {
        let mut buf = vec![0u8; MAX_FRAME];
        let n = loop {
            let ret = unsafe {
                libc::read(
                    self.read_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if ret < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            break ret as usize;
        };
        if n == 0 {
            return;
        }
        let mut frames = self.rx_frames.lock().unwrap();
        if frames.len() >= FRAME_CAP {
            frames.remove(0);
        }
        frames.push(buf[..n].to_vec());
    }
}

impl MutEventSubscriber for NetRxHandler {
    fn init(&mut self, ops: &mut EventOps) {
        let events = Events::new_raw(self.read_fd, EventSet::IN);
        if ops.add(events).is_err() {
            // fd may already be registered (edge case), continue.
        }
    }

    fn process(&mut self, events: Events, _ops: &mut EventOps) {
        if events.event_set() == EventSet::IN {
            for _ in 0..8 {
                self.read_and_enqueue();
            }
        }
    }
}

pub struct Net {
    tap_fd: RawFd,
    rx_frames: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Net {
    pub fn new_tap(read_fd: RawFd, write_fd: RawFd) -> io::Result<Self> {
        set_nonblocking(read_fd)?;
        let rx = Arc::new(Mutex::new(Vec::with_capacity(FRAME_CAP)));
        let rx2 = rx.clone();

        let handler = NetRxHandler {
            read_fd,
            rx_frames: rx2,
        };
        let mut mgr =
            EventManager::new().map_err(|e| io::Error::other(format!("event-manager: {e}")))?;
        mgr.add_subscriber(handler);

        thread::spawn(move || loop {
            match mgr.run_with_timeout(200) {
                Ok(_) => {}
                Err(event_manager::Error::Epoll(_)) => break,
                _ => {}
            }
        });

        Ok(Net {
            tap_fd: write_fd,
            rx_frames: rx,
        })
    }

    fn process_rx(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let written = self.fill_rx(chain, mem);
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        if used {
            ISR_USED_BUFFER
        } else {
            0
        }
    }

    fn fill_rx(
        &mut self,
        chain: virtio_queue::DescriptorChain<&GuestMemoryMmap>,
        mem: &GuestMemoryMmap,
    ) -> u32 {
        let frame = match self.rx_frames.lock().unwrap().pop() {
            Some(f) => f,
            None => return 0,
        };
        let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
        if descs.is_empty() {
            return 0;
        }

        let first_addr = descs[0].addr();
        let _ = mem.write_slice(&[0u8; NET_HDR_SIZE], first_addr);

        let mut copied = 0usize;
        let mut num_bufs: u16 = 0;

        for desc in &descs {
            if !desc.is_write_only() {
                continue;
            }
            let avail = desc.len() as usize;
            if avail == 0 {
                continue;
            }
            let remaining = frame.len() - copied;
            let chunk = cmp::min(avail, remaining);
            let _ = mem.write_slice(&frame[copied..copied + chunk], desc.addr());
            copied += chunk;
            num_bufs += 1;
            if copied >= frame.len() {
                break;
            }
        }

        let _ = mem.write_obj(num_bufs.to_le_bytes(), GuestAddress(first_addr.0 + 10));

        (NET_HDR_SIZE + copied) as u32
    }

    fn process_tx(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
            let mut is_first = true;

            for desc in &descs {
                if desc.is_write_only() {
                    continue;
                }
                let (start_offset, data_len) = if is_first {
                    is_first = false;
                    let total = desc.len() as usize;
                    if total <= NET_HDR_SIZE {
                        continue;
                    }
                    (NET_HDR_SIZE, total - NET_HDR_SIZE)
                } else {
                    (0, desc.len() as usize)
                };
                if data_len == 0 {
                    continue;
                }
                let mut frame = vec![0u8; data_len];
                let addr = GuestAddress(desc.addr().0 + start_offset as u64);
                if mem.read_slice(&mut frame, addr).is_ok() {
                    write_all(self.tap_fd, &frame);
                }
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

impl VirtioDevice for Net {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_NET
    }
    fn features(&self) -> u64 {
        VIRTIO_NET_F_MAC | VIRTIO_NET_F_MRG_RXBUF | VIRTIO_NET_F_STATUS
    }
    fn queue_count(&self) -> usize {
        2
    }
    fn queue_max_size(&self) -> u16 {
        256
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 12];
        cfg[0..6].copy_from_slice(&MAC_ADDR);
        cfg[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
        cfg[8..10].copy_from_slice(&MTU.to_le_bytes());
        let s = offset as usize;
        let e = (s + data.len()).min(12);
        data[..e - s].copy_from_slice(&cfg[s..e]);
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
        self.rx_frames.lock().unwrap().clear();
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn write_all(fd: RawFd, buf: &[u8]) {
    let mut off = 0;
    while off < buf.len() {
        let ret = unsafe {
            libc::write(
                fd,
                buf[off..].as_ptr() as *const libc::c_void,
                buf.len() - off,
            )
        };
        if ret <= 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted && e.kind() != io::ErrorKind::WouldBlock {
                break;
            }
            continue;
        }
        off += ret as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sp() -> (RawFd, RawFd) {
        let mut f = [-1i32; 2];
        unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, f.as_mut_ptr());
        }
        (f[0], f[1])
    }
    #[test]
    fn test_identity() {
        let (r, w) = sp();
        let n = Net::new_tap(r, w).unwrap();
        assert_eq!(1, n.device_id());
        assert_eq!(2, n.queue_count());
    }
    #[test]
    fn test_mac() {
        let (r, w) = sp();
        let n = Net::new_tap(r, w).unwrap();
        let mut m = [0u8; 6];
        n.read_config(0, &mut m);
        assert_eq!(MAC_ADDR, m);
    }
    #[test]
    fn test_config() {
        let (r, w) = sp();
        let n = Net::new_tap(r, w).unwrap();
        let mut c = [0u8; 12];
        n.read_config(0, &mut c);
        assert_eq!(1u16, u16::from_le_bytes(c[6..8].try_into().unwrap()));
    }
}

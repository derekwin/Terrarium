//! virtio-net 设备（M1.5 Task 0）。
//!
//! device_id=1，rx/tx 双队列。features=VIRTIO_NET_F_MAC。
//! config space：6 字节 MAC 地址。
//! 收包：独立读线程 → 帧队列 → queue_notify 填 guest rx 描述符。
//! 发包：queue_notify 同步 write 到后端 fd。

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::virtio_mmio::{VirtioDevice, ISR_USED_BUFFER};

const VIRTIO_ID_NET: u32 = 1;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const NET_HDR_SIZE: usize = 10;
const RX_FRAME_CAPACITY: usize = 64;

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

enum NetBackend {
    TapFd {
        #[allow(dead_code)]
        read_fd: File,
        write_fd: File,
    },
    #[allow(dead_code)]
    UnixSocket { stream: UnixStream },
}

pub struct Net {
    mac: [u8; 6],
    backend: NetBackend,
    rx_frames: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Net {
    pub fn new_tap(tap_read_fd: i32, tap_write_fd: i32) -> io::Result<Self> {
        // SAFETY: caller owns these fds.
        #[allow(unsafe_code)]
        let read_fd = unsafe { File::from_raw_fd(tap_read_fd) };
        #[allow(unsafe_code)]
        let write_fd = unsafe { File::from_raw_fd(tap_write_fd) };

        let rx_frames = Arc::new(Mutex::new(Vec::with_capacity(RX_FRAME_CAPACITY)));
        let mut read_clone = read_fd.try_clone()?;

        let rx = rx_frames.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                match read_clone.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut frames = rx.lock().unwrap();
                        if frames.len() >= RX_FRAME_CAPACITY {
                            frames.remove(0);
                        }
                        frames.push(buf[..n].to_vec());
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Net {
            mac: [0x02, 0x54, 0x45, 0x52, 0x52, 0x41],
            backend: NetBackend::TapFd { read_fd, write_fd },
            rx_frames,
        })
    }

    fn process_rx(&mut self, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        let mut used_any = false;

        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();
            if descs.is_empty() {
                let _ = queue.add_used(mem, head, 0);
                used_any = true;
                continue;
            }

            let frame = {
                let mut frames = self.rx_frames.lock().unwrap();
                if frames.is_empty() {
                    break;
                }
                frames.remove(0)
            };

            let buf_addr = descs[0].addr();
            let buf_len = descs[0].len() as usize;
            let total = NET_HDR_SIZE + frame.len();

            if total <= buf_len {
                let _ = mem.write_slice(&[0u8; NET_HDR_SIZE], buf_addr);
                let _ = mem.write_slice(&frame, GuestAddress(buf_addr.0 + NET_HDR_SIZE as u64));
            }
            let _ = queue.add_used(mem, head, total as u32);
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
            let descs: Vec<virtio_queue::desc::split::Descriptor> = chain.collect();

            for desc in &descs {
                let len = desc.len() as usize;
                if len <= NET_HDR_SIZE {
                    continue;
                }
                let payload_addr = GuestAddress(desc.addr().0 + NET_HDR_SIZE as u64);
                let mut frame = vec![0u8; len - NET_HDR_SIZE];
                if mem.read_slice(&mut frame, payload_addr).is_ok() {
                    match &mut self.backend {
                        NetBackend::TapFd { write_fd, .. } => {
                            let _ = write_fd.write_all(&frame);
                        }
                        NetBackend::UnixSocket { stream } => {
                            let _ = stream.write_all(&frame);
                        }
                    }
                }
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

impl VirtioDevice for Net {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_NET
    }

    fn features(&self) -> u64 {
        VIRTIO_NET_F_MAC
    }

    fn queue_count(&self) -> usize {
        2
    }

    fn queue_max_size(&self) -> u16 {
        256
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        if offset < 6 {
            let start = offset as usize;
            let end = (start + data.len()).min(6);
            data[..end - start].copy_from_slice(&self.mac[start..end]);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn queue_notify(&mut self, qi: usize, queue: &mut Queue, mem: &GuestMemoryMmap) -> u32 {
        match qi {
            RX_QUEUE => self.process_rx(queue, mem),
            TX_QUEUE => self.process_tx(queue, mem),
            _ => 0,
        }
    }

    fn reset(&mut self) {
        self.rx_frames.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unsafe_code)]
    fn socketpair() -> (i32, i32) {
        let mut fds = [-1i32; 2];
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0);
        (fds[0], fds[1])
    }

    #[test]
    fn test_device_identity() {
        let (rfd, wfd) = socketpair();
        let net = Net::new_tap(rfd, wfd).unwrap();
        assert_eq!(1, net.device_id());
        assert_eq!(VIRTIO_NET_F_MAC, net.features());
        assert_eq!(2, net.queue_count());
        assert_eq!(256, net.queue_max_size());
        // RX thread holds cloned fd, drop won't close the original.
    }

    #[test]
    fn test_mac_config() {
        let (rfd, wfd) = socketpair();
        let net = Net::new_tap(rfd, wfd).unwrap();
        let mut mac = [0u8; 6];
        net.read_config(0, &mut mac);
        assert_eq!([0x02, 0x54, 0x45, 0x52, 0x52, 0x41], mac);
    }
}

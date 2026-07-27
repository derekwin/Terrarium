//! Minimal AF_VSOCK listener via libc (no external crate needed).

use std::io::{Error, ErrorKind};

const AF_VSOCK: i32 = 40;
const VMADDR_CID_ANY: u32 = u32::MAX;

#[repr(C)]
struct SockaddrVm {
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_zero: [u8; 4],
}

/// Bind and listen on a vsock port (any CID).
pub fn listen(port: u32) -> std::io::Result<i32> {
    // SAFETY: plain socket(2)/bind(2)/listen(2) calls with a correctly
    // initialized sockaddr_vm; no pointers into Rust memory are retained.
    unsafe {
        let fd = libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        let addr = SockaddrVm {
            svm_family: AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: VMADDR_CID_ANY,
            svm_zero: [0; 4],
        };
        let ret = libc::bind(
            fd,
            &addr as *const SockaddrVm as *const libc::sockaddr,
            std::mem::size_of::<SockaddrVm>() as u32,
        );
        if ret < 0 {
            let e = Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        if libc::listen(fd, 16) < 0 {
            let e = Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(fd)
    }
}

/// Accept one connection from a vsock listener fd.
pub fn accept(listen_fd: i32) -> std::io::Result<i32> {
    // SAFETY: accept(2) on a valid listening fd with null addr (we don't
    // need the peer address).
    unsafe {
        let fd = libc::accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut());
        if fd < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(fd)
        }
    }
}

use std::os::unix::io::FromRawFd;
pub fn from_raw_fd_checked(fd: i32) -> std::io::Result<std::fs::File> {
    if fd < 0 {
        return Err(Error::new(ErrorKind::InvalidInput, "negative fd"));
    }
    // SAFETY: fd is a valid, owned socket fd from accept(2).
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

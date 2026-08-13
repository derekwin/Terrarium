//! Supervisor loop: installs the confinement in a child (cgroup memory +
//! Landlock fs + seccomp user-notify for network syscalls), then handles
//! network-connect verdicts against the whitelist. Denials are written to
//! the denyfd channel (guest-proxy maps them to the structured exit code).

use crate::cgroup;
use crate::landlock::LandlockGuard;
use crate::netpolicy;
use crate::{Config, SANDBOX_DENY_EXIT_CODE};
use std::os::fd::RawFd;

// ── seccomp ioctls (linux/seccomp.h) ────────────────────────────────────────
const SECCOMP_SET_MODE_FILTER: libc::c_int = 1;
// Terrarium's guest kernel uses custom seccomp flag values (linux-6.12
// include/uapi/linux/seccomp.h: NEW_LISTENER = 1<<3), matching sandlock.
// The standard upstream value (1<<15) is rejected with EINVAL here.
const SECCOMP_FILTER_FLAG_NEW_LISTENER: u32 = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;

const SECCOMP_IOCTL_NOTIF_RECV: u64 = (3u32 << 30 | 80u32 << 16 | 0x21u32 << 8 | 0) as u64; // _IOWR('!',0,notif)
const SECCOMP_IOCTL_NOTIF_SEND: u64 = (3u32 << 30 | 24u32 << 16 | 0x21u32 << 8 | 1) as u64; // _IOWR('!',1,resp)
const SECCOMP_IOCTL_NOTIF_ID_VALID: u64 = (2u32 << 30 | 8u32 << 16 | 0x21u32 << 8 | 2) as u64; // _IOR('!',2,u64)
                                                                                               // SECCOMP_USER_NOTIF_FLAG_CONTINUE — respond "proceed" without replaying
                                                                                               // the syscall (replay would re-enter the notifier and loop).
const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;

#[repr(C)]
struct SeccompData {
    nr: libc::c_int,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

#[repr(C)]
struct SeccompNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: SeccompData,
}

#[repr(C)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

// ── BPF (classic, loadable via SECCOMP_SET_MODE_FILTER) ─────────────────────
const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: libc::c_ushort,
    filter: *const SockFilter,
}

const NET_SYSCALLS: &[i64] = &[
    libc::SYS_connect,
    libc::SYS_sendto,
    libc::SYS_sendmsg,
    libc::SYS_sendmmsg,
];

/// Classic BPF: load `seccomp_data.nr` and jump to USER_NOTIF for each
/// network syscall, otherwise ALLOW.
fn build_bpf() -> Vec<SockFilter> {
    // offsetof(seccomp_data, nr) == 0
    let mut insns = vec![SockFilter {
        code: BPF_LD | BPF_W | BPF_ABS,
        jt: 0,
        jf: 0,
        k: 0,
    }];
    for nr in NET_SYSCALLS {
        // JEQ nr: true → fall through to RET_USER_NOTIF (jt=0); false →
        // skip the RET (jf=1) to the next JEQ (or the final RET_ALLOW).
        insns.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 0,
            jf: 1,
            k: *nr as u32,
        });
        insns.push(SockFilter {
            code: BPF_RET,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_USER_NOTIF,
        });
    }
    insns.push(SockFilter {
        code: BPF_RET,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ALLOW,
    });
    insns
}

// ── denyfd channel ──────────────────────────────────────────────────────────
fn deny_writer() -> Result<RawFd, String> {
    if let Ok(v) = std::env::var("SANDBOX_DENY_FD") {
        let fd: i32 = v.parse().map_err(|_| "bad SANDBOX_DENY_FD")?;
        make_nonblocking(fd);
        return Ok(fd);
    }
    let mut fds = [0i32; 2];
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if ret != 0 {
        return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
    }
    make_nonblocking(fds[1]);
    Ok(fds[1])
}

fn make_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn record_deny(fd: RawFd, syscall: &str) {
    let line = format!("{{\"syscall\":\"{syscall}\",\"errno\":1}}\n");
    // Non-blocking best-effort: the guest-proxy drains the denyfd channel
    // only when the exec returns, so a chatty child (e.g. wget retrying a
    // denied connect) can fill the pipe. A full pipe must never block the
    // supervisor (which would wedge the child) — drop records instead.
    unsafe {
        let _ = libc::write(fd, line.as_ptr() as *const libc::c_void, line.len());
    }
}

// ── child-side confinement ──────────────────────────────────────────────────
fn confine_child(cfg: &Config, notif_pipe_w: RawFd) -> Result<(), String> {
    if let Some(mb) = cfg.memory_mb {
        cgroup::apply_memory_limit(mb)?;
    }
    // Landlock restricts self (irreversible) before seccomp install.
    let _ll = LandlockGuard::install(&cfg.read_paths, &cfg.write_paths)?;

    let bpf = build_bpf();
    let prog = SockFprog {
        len: bpf.len() as libc::c_ushort,
        filter: bpf.as_ptr(),
    };
    // no_new_privs (Landlock wants it; harmless as root)
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    let listener = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER as libc::c_ulong,
            &prog,
        )
    };
    if listener < 0 {
        return Err(format!(
            "seccomp NEW_LISTENER: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Pass the listener fd NUMBER to the parent over a plain pipe (write
    // only — never sendmsg, which the seccomp filter above would trap as
    // USER_NOTIF and deadlock on).
    let val = listener as u32;
    let ret = unsafe {
        libc::write(
            notif_pipe_w,
            &val as *const u32 as *const libc::c_void,
            std::mem::size_of::<u32>(),
        )
    };
    unsafe {
        libc::close(notif_pipe_w);
        // Keep the listener fd open: the parent duplicates it via
        // pidfd_getfd before the child execs.
    }
    if ret < 0 {
        return Err(format!(
            "write notif fd: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

// ── supervisor-side verdicts ────────────────────────────────────────────────
fn read_sockaddr(pid: u32, ptr: u64) -> Option<(std::net::IpAddr, u16)> {
    if ptr == 0 {
        return None;
    }
    let mut buf = [0u8; 28];
    let mut local = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: ptr as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let ret = unsafe { libc::process_vm_readv(pid as libc::pid_t, &mut local, 1, &remote, 1, 0) };
    if ret < 0 {
        return None;
    }
    let family = u16::from_ne_bytes([buf[0], buf[1]]);
    match family {
        n if n == libc::AF_INET as u16 => {
            let port = u16::from_be_bytes([buf[2], buf[3]]);
            let ip = std::net::Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
            Some((std::net::IpAddr::V4(ip), port))
        }
        n if n == libc::AF_INET6 as u16 => {
            let port = u16::from_be_bytes([buf[2], buf[3]]);
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[8..24]);
            Some((std::net::IpAddr::V6(octets.into()), port))
        }
        _ => None,
    }
}

fn syscall_name(nr: i64) -> &'static str {
    match nr {
        n if n == libc::SYS_connect => "connect",
        n if n == libc::SYS_sendto => "sendto",
        n if n == libc::SYS_sendmsg => "sendmsg",
        n if n == libc::SYS_sendmmsg => "sendmmsg",
        _ => "net",
    }
}

enum Verdict {
    Allow,                         // no destination (connected socket data)
    Target(std::net::IpAddr, u16), // destination to match against policy
    Deny,                          // unreadable/unknown → fail closed
}

fn notif_verdict(notif: &SeccompNotif) -> Verdict {
    let nr = notif.data.nr as i64;
    match nr {
        n if n == libc::SYS_connect => match read_sockaddr(notif.pid, notif.data.args[1]) {
            Some(t) => Verdict::Target(t.0, t.1),
            None => Verdict::Deny,
        },
        n if n == libc::SYS_sendto => {
            // addrlen == 0 → connected socket, no destination: the
            // connect was already adjudicated; allow data.
            if notif.data.args[5] == 0 {
                Verdict::Allow
            } else {
                match read_sockaddr(notif.pid, notif.data.args[4]) {
                    Some(t) => Verdict::Target(t.0, t.1),
                    None => Verdict::Deny,
                }
            }
        }
        n if n == libc::SYS_sendmsg || n == libc::SYS_sendmmsg => {
            match read_msghdr_target(notif.pid, notif.data.args[1]) {
                Some(Some(t)) => Verdict::Target(t.0, t.1),
                Some(None) => Verdict::Allow, // msg_namelen == 0
                None => Verdict::Deny,
            }
        }
        _ => Verdict::Deny,
    }
}

/// Read `msghdr.msg_name` / `msg_namelen` from the child. Returns
/// `Ok(None)` when msg_namelen == 0 (connected socket, no destination).
/// x86_64 `struct msghdr` layout: msg_name at 0, msg_namelen at 8.
fn read_msghdr_target(pid: u32, msghdr_ptr: u64) -> Option<Option<(std::net::IpAddr, u16)>> {
    if msghdr_ptr == 0 {
        return None;
    }
    let mut buf = [0u8; 16];
    let mut local = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let remote = libc::iovec {
        iov_base: msghdr_ptr as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let ret = unsafe { libc::process_vm_readv(pid as libc::pid_t, &mut local, 1, &remote, 1, 0) };
    if ret < 0 {
        return None;
    }
    let name_ptr = u64::from_ne_bytes(buf[0..8].try_into().ok()?);
    let name_len = u64::from_ne_bytes(buf[8..16].try_into().ok()?);
    if name_len == 0 {
        return Some(None);
    }
    if name_ptr == 0 {
        return None;
    }
    read_sockaddr(pid, name_ptr).map(Some)
}

fn notify_send(listener: RawFd, id: u64, error: i32, flags: u32) -> bool {
    let resp = SeccompNotifResp {
        id,
        val: 0,
        error,
        flags,
    };
    // ioctl via syscall: libc's ioctl signature differs between gnu
    // (c_ulong) and musl (c_int) — syscall keeps one code path.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            listener as u64,
            SECCOMP_IOCTL_NOTIF_SEND,
            &resp as *const SeccompNotifResp as u64,
        )
    };
    ret == 0
}

fn notify_valid(listener: RawFd, id: u64) -> bool {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            listener as u64,
            SECCOMP_IOCTL_NOTIF_ID_VALID,
            &id as *const u64 as u64,
        )
    };
    ret == 0
}

/// Run the confined command. Returns the child exit code.
pub(crate) fn run(cfg: &Config) -> Result<i32, String> {
    let rules = netpolicy::parse(&cfg.net_allow)?;
    let deny_fd = deny_writer()?;

    // Plain pipe for handing the seccomp listener fd number to the
    // supervisor (write-only; sendmsg would be trapped by the filter).
    let mut sp = [0i32; 2];
    let ret = unsafe { libc::pipe2(sp.as_mut_ptr(), libc::O_CLOEXEC) };
    if ret != 0 {
        return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // child
        unsafe {
            libc::close(sp[0]);
        }
        if let Err(e) = confine_child(cfg, sp[1]) {
            eprintln!("terra-sandbox child: {e}");
            unsafe { libc::_exit(1) };
        }
        let argv: Vec<std::ffi::CString> = cfg
            .cmd
            .iter()
            .map(|s| std::ffi::CString::new(s.as_str()).unwrap())
            .collect();
        let mut c_args: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        c_args.push(std::ptr::null());
        unsafe {
            // execvp resolves argv[0] through PATH (execv would need an
            // absolute path).
            libc::execvp(c_args[0], c_args.as_ptr());
            let e = std::io::Error::last_os_error();
            eprintln!("terra-sandbox exec: {e}");
            libc::_exit(127);
        }
    }

    // parent supervisor
    unsafe {
        libc::close(sp[1]);
    }
    let listener = recv_listener_fd(sp[0], pid);
    unsafe {
        libc::close(sp[0]);
    }
    let listener = listener?;

    loop {
        let mut notif: SeccompNotif = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::syscall(
                libc::SYS_ioctl,
                listener as u64,
                SECCOMP_IOCTL_NOTIF_RECV,
                &mut notif as *mut SeccompNotif as u64,
            )
        };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                break; // no more targets
            }
            return Err(format!("NOTIF_RECV: {err}"));
        }
        let allowed = match notif_verdict(&notif) {
            Verdict::Allow => true,
            Verdict::Target(ip, port) => netpolicy::allows(&rules, ip, port),
            Verdict::Deny => false,
        };
        if allowed {
            if notify_valid(listener, notif.id) {
                notify_send(listener, notif.id, 0, SECCOMP_USER_NOTIF_FLAG_CONTINUE);
            }
        } else {
            let name = syscall_name(notif.data.nr as i64);
            record_deny(deny_fd, name);
            if notify_valid(listener, notif.id) {
                notify_send(listener, notif.id, -libc::EPERM, 0);
            }
        }
    }

    let mut status: libc::c_int = 0;
    let w = unsafe { libc::waitpid(pid, &mut status, 0) };
    if w < 0 {
        return Err(format!("waitpid: {}", std::io::Error::last_os_error()));
    }
    if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        // guest-proxy maps denyfd+nonzero to SANDBOX_DENY_EXIT_CODE; keep
        // the raw code here.
        Ok(code)
    } else {
        Ok(SANDBOX_DENY_EXIT_CODE)
    }
}

fn recv_listener_fd(pipe_fd: RawFd, child_pid: libc::pid_t) -> Result<RawFd, String> {
    let mut val: u32 = 0;
    let ret = unsafe {
        libc::read(
            pipe_fd,
            &mut val as *mut u32 as *mut libc::c_void,
            std::mem::size_of::<u32>(),
        )
    };
    if ret != std::mem::size_of::<u32>() as isize {
        return Err("read notif fd: short read or error".into());
    }
    // The fd number lives in the child's table — duplicate it here.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child_pid as u64, 0) };
    if pidfd < 0 {
        return Err(format!("pidfd_open: {}", std::io::Error::last_os_error()));
    }
    let dup = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd as u64, val as u64, 0) };
    unsafe {
        libc::close(pidfd as RawFd);
    }
    if dup < 0 {
        return Err(format!("pidfd_getfd: {}", std::io::Error::last_os_error()));
    }
    Ok(dup as RawFd)
}

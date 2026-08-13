//! Landlock ruleset: default-deny filesystem with explicit grants.
//! Kernel-enforced (no supervisor involvement) — the whole filesystem
//! policy costs ~1.3x vs bare execution.

use std::ffi::CString;
use std::os::fd::RawFd;

const LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const LANDLOCK_ADD_RULE: libc::c_long = 445;
const LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;

pub(crate) const HANDLED_FS: u64 = FS_EXECUTE
    | FS_WRITE_FILE
    | FS_READ_FILE
    | FS_READ_DIR
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM;

const READ_GRANT: u64 = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
const RW_GRANT: u64 = READ_GRANT
    | FS_WRITE_FILE
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: RawFd,
}

pub(crate) struct LandlockGuard(RawFd);

impl LandlockGuard {
    /// Build the ruleset from grants, restrict the calling process, and
    /// return a guard that closes the ruleset fd on drop.
    pub fn install(read_paths: &[String], write_paths: &[String]) -> Result<Self, String> {
        let attr = RulesetAttr {
            handled_access_fs: HANDLED_FS,
            handled_access_net: 0,
        };
        let fd = unsafe {
            libc::syscall(
                LANDLOCK_CREATE_RULESET,
                &attr,
                std::mem::size_of::<RulesetAttr>(),
                0,
            )
        };
        if fd < 0 {
            return Err(format!(
                "landlock_create_ruleset: {}",
                std::io::Error::last_os_error()
            ));
        }
        let guard = Self(fd as RawFd);
        for p in read_paths {
            add_path_rule(guard.0, p, READ_GRANT)?;
        }
        for p in write_paths {
            add_path_rule(guard.0, p, RW_GRANT)?;
        }
        let ret = unsafe { libc::syscall(LANDLOCK_RESTRICT_SELF, guard.0, 0) };
        if ret != 0 {
            return Err(format!(
                "landlock_restrict_self: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(guard)
    }
}

impl Drop for LandlockGuard {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

fn add_path_rule(ruleset: RawFd, path: &str, access: u64) -> Result<(), String> {
    let c = CString::new(path).map_err(|_| format!("bad path {path:?}"))?;
    let dir_fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if dir_fd < 0 {
        // Grant paths that don't exist are skipped (mirrors sandlock's
        // exists-filtering for alpine which lacks /lib64 and /sbin).
        return Ok(());
    }
    // Landlock path-beneath rules apply to directories. Non-directory
    // grants (e.g. /dev/null, /dev/urandom) cannot be expressed precisely
    // — skipping them keeps the whole /dev tree default-deny (devices are
    // a Device capability, unsupported in v1).
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let fstat_ok = unsafe { libc::fstat(dir_fd, &mut st) } == 0;
    if !fstat_ok || (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        unsafe {
            libc::close(dir_fd);
        }
        return Ok(());
    }
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: dir_fd,
    };
    let ret = unsafe {
        libc::syscall(
            LANDLOCK_ADD_RULE,
            ruleset,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr,
            0,
        )
    };
    unsafe {
        libc::close(dir_fd);
    }
    if ret != 0 {
        return Err(format!(
            "landlock_add_rule {path}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

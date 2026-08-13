//! cgroup v2 memory cap for a sandbox process tree. The guest kernel has
//! cpuset/cpu/io/memory controllers (no pids — process count is bounded by
//! the VM quota; cgroup pids needs a kernel rebuild and is future work).

use std::fs;
use std::path::PathBuf;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Ensure cgroup2 is mounted (guest is root; VM-scoped, idempotent).
fn ensure_mounted() -> Result<(), String> {
    if PathBuf::from(format!("{CGROUP_ROOT}/cgroup.controllers")).exists() {
        return Ok(());
    }
    fs::create_dir_all(CGROUP_ROOT).map_err(|e| format!("mkdir {CGROUP_ROOT}: {e}"))?;
    let ret = unsafe {
        libc::mount(
            b"cgroup2\0".as_ptr() as *const libc::c_char,
            b"/sys/fs/cgroup\0".as_ptr() as *const libc::c_char,
            b"cgroup2\0".as_ptr() as *const libc::c_char,
            0,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        return Err(format!(
            "mount cgroup2: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Move this process (and its future children) into a fresh child cgroup
/// with memory.max = `mb`. Must run before fork so the whole tree is
/// counted.
pub(crate) fn apply_memory_limit(mb: u64) -> Result<(), String> {
    ensure_mounted()?;
    let dir = PathBuf::from(format!("{CGROUP_ROOT}/terra-sb-{}", std::process::id()));
    fs::create_dir(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    fs::write(dir.join("memory.max"), mb.to_string())
        .map_err(|e| format!("write memory.max: {e}"))?;
    fs::write(dir.join("cgroup.procs"), std::process::id().to_string())
        .map_err(|e| format!("join cgroup: {e}"))?;
    Ok(())
}

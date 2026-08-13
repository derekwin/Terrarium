//! cgroup v2 resource limits for a sandbox process tree: memory.max,
//! cpu.weight (from cpu_shares) and pids.max (needs CONFIG_CGROUP_PIDS in
//! the guest kernel).

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

pub(crate) struct Limits {
    pub memory_mb: Option<u64>,
    pub cpu_shares: Option<u64>,
    pub procs: Option<u32>,
}

/// Move this process (and its future children) into a fresh child cgroup
/// and apply the requested limits. Must run before fork so the whole tree
/// is counted.
pub(crate) fn apply_limits(lim: &Limits) -> Result<(), String> {
    if lim.memory_mb.is_none() && lim.cpu_shares.is_none() && lim.procs.is_none() {
        return Ok(());
    }
    ensure_mounted()?;
    enable_controllers()?;
    let dir = PathBuf::from(format!("{CGROUP_ROOT}/terra-sb-{}", std::process::id()));
    fs::create_dir(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    if let Some(mb) = lim.memory_mb {
        // cgroup v2 memory.max is in BYTES; the policy's memory_mb is
        // megabytes. Writing the raw number would cap the cgroup at a few
        // bytes and OOM-kill the very first allocation.
        let bytes = mb
            .checked_mul(1024 * 1024)
            .ok_or_else(|| format!("memory_mb {mb} overflows bytes"))?;
        fs::write(dir.join("memory.max"), bytes.to_string())
            .map_err(|e| format!("write memory.max: {e}"))?;
    }
    if let Some(shares) = lim.cpu_shares {
        // cgroup v2 cpu.weight: 1..=10000, default 100 (cpu_shares 1024).
        let weight = (100u64 * shares / 1024).clamp(1, 10000);
        fs::write(dir.join("cpu.weight"), weight.to_string())
            .map_err(|e| format!("write cpu.weight: {e}"))?;
    }
    if let Some(p) = lim.procs {
        fs::write(dir.join("pids.max"), p.to_string())
            .map_err(|e| format!("write pids.max: {e} (guest kernel lacks CONFIG_CGROUP_PIDS?)"))?;
    }
    fs::write(dir.join("cgroup.procs"), std::process::id().to_string())
        .map_err(|e| format!("join cgroup: {e}"))?;
    Ok(())
}

/// Enable the controllers we use in the root cgroup's subtree so child
/// cgroups can set memory/cpu/pids limits (idempotent).
fn enable_controllers() -> Result<(), String> {
    let existing =
        fs::read_to_string(format!("{CGROUP_ROOT}/cgroup.subtree_control")).unwrap_or_default();
    let mut add = String::new();
    for c in ["memory", "cpu", "pids"] {
        if !existing.split_whitespace().any(|w| w == c) {
            add.push_str(&format!("+{c} "));
        }
    }
    if !add.is_empty() {
        fs::write(format!("{CGROUP_ROOT}/cgroup.subtree_control"), add.trim())
            .map_err(|e| format!("enable cgroup controllers: {e}"))?;
    }
    Ok(())
}

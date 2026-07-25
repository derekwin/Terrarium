//! cgroup v2 resource control — per-sandbox limits.
//!
//! Creates cgroups under /sys/fs/cgroup/terra-<name>/ and applies
//! memory.max and cpu.weight limits. Cleaner and faster than seccomp
//! notif-based resource simulation (requires root / CAP_SYS_ADMIN).

use std::fs;

/// A cgroup v2 container for a sandbox.
#[allow(dead_code)]
pub struct Cgroup {
    name: String,
    path: String,
}

impl Cgroup {
    #[allow(dead_code)]
    pub fn create(
        name: &str,
        memory_mb: Option<u64>,
        cpu_shares: Option<u64>,
    ) -> Result<Self, String> {
        let path = format!("/sys/fs/cgroup/terra-{}", name);
        fs::create_dir_all(&path).map_err(|e| format!("mkdir cgroup {}: {}", path, e))?;

        // Move current process into cgroup before applying limits
        fs::write(
            format!("{}/cgroup.procs", path),
            std::process::id().to_string(),
        )
        .map_err(|e| format!("write cgroup.procs: {}", e))?;

        if let Some(mb) = memory_mb {
            let limit = mb * 1024 * 1024;
            fs::write(format!("{}/memory.max", path), limit.to_string())
                .map_err(|e| format!("write memory.max: {}", e))?;
            tracing::info!(%name, memory_mb = mb, "cgroup memory limit set");
        }

        if let Some(shares) = cpu_shares {
            fs::write(format!("{}/cpu.weight", path), shares.to_string())
                .map_err(|e| format!("write cpu.weight: {}", e))?;
            tracing::info!(%name, cpu_shares = shares, "cgroup cpu weight set");
        }

        Ok(Self {
            name: name.to_string(),
            path,
        })
    }

    /// Destroy the cgroup (processes must have exited or been migrated).
    #[allow(dead_code)]
    pub fn destroy(&self) -> Result<(), String> {
        fs::remove_dir(&self.path).ok(); // Best-effort — may fail if processes still exist
        tracing::info!(name = %self.name, "cgroup destroyed");
        Ok(())
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

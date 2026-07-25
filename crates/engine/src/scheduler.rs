//! Resource scheduler — admission control, placement, idle reclamation.

#![allow(dead_code)]

use crate::manager::VmManager;

/// Tracked host resource state.
#[derive(Debug, Clone)]
pub struct HostResources {
    pub cpu_total: u32,
    pub cpu_used: u32,
    pub memory_total_mb: u64,
    pub memory_used_mb: u64,
}

impl HostResources {
    pub fn new() -> Self {
        let cpu_total = num_cpus::get() as u32;
        let mem_total = detect_memory_mb();
        Self {
            cpu_total,
            cpu_used: 0,
            memory_total_mb: mem_total,
            memory_used_mb: 0,
        }
    }

    /// Available CPU count.
    pub fn cpu_available(&self) -> u32 {
        self.cpu_total.saturating_sub(self.cpu_used)
    }
    /// Available memory in MB.
    pub fn memory_available_mb(&self) -> u64 {
        self.memory_total_mb.saturating_sub(self.memory_used_mb)
    }
}

/// Admission control + resource tracking.
pub struct Scheduler {
    resources: HostResources,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            resources: HostResources::new(),
        }
    }

    /// Check if a new VM fits in remaining host resources.
    pub fn can_allocate(&self, cpu: u32, memory_mb: u64) -> bool {
        self.resources.cpu_available() >= cpu && self.resources.memory_available_mb() >= memory_mb
    }

    /// Reserve resources for a new VM.
    pub fn allocate(&mut self, cpu: u32, memory_mb: u64) {
        self.resources.cpu_used += cpu;
        self.resources.memory_used_mb += memory_mb;
        tracing::info!(
            cpu_used = self.resources.cpu_used,
            mem_used_mb = self.resources.memory_used_mb,
            "Allocated"
        );
    }

    /// Release resources when a VM is destroyed.
    pub fn deallocate(&mut self, cpu: u32, memory_mb: u64) {
        self.resources.cpu_used = self.resources.cpu_used.saturating_sub(cpu);
        self.resources.memory_used_mb = self.resources.memory_used_mb.saturating_sub(memory_mb);
        tracing::info!(
            cpu_used = self.resources.cpu_used,
            mem_used_mb = self.resources.memory_used_mb,
            "Deallocated"
        );
    }

    /// Scale down idle VMs to minimal config.
    /// Called periodically by engine loop.
    pub fn reclaim_idle(&self, mgr: &mut VmManager) {
        for vm in mgr.list() {
            if let Ok(info) = vm.info() {
                let boot_cpus = info
                    .config
                    .as_ref()
                    .and_then(|c| c.cpus.as_ref())
                    .map(|c| c.boot)
                    .unwrap_or(1);
                if boot_cpus > 1 {
                    let _ = vm.resize_vcpus(Some(1));
                    tracing::info!(name = %vm.name(), "Reclaimed idle vCPUs");
                }
            }
        }
    }

    /// Current resource state.
    pub fn state(&self) -> &HostResources {
        &self.resources
    }
}

fn detect_memory_mb() -> u64 {
    let mut mem = 1024u64; // safe default
    if let Ok(data) = std::fs::read_to_string("/proc/meminfo") {
        for line in data.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(kb) = line.split_whitespace().nth(1) {
                    if let Ok(val) = kb.parse::<u64>() {
                        mem = val / 1024; // KB → MB
                    }
                }
                break;
            }
        }
    }
    mem
}

//! Placement decisions for VM and sandbox scheduling.

#![allow(dead_code)]

#[allow(dead_code)]
pub struct PlacementDecision {
    /// Target node (hostname or IP).
    pub node: String,
    /// Number of VMs already running on this node.
    pub vm_count: usize,
    /// Remaining CPU / memory on this node.
    pub cpu_available: u32,
    pub memory_available_mb: u64,
}

#[allow(dead_code)]
pub struct PlacementEngine;

impl PlacementEngine {
    /// M2: always returns localhost. M3: picks best node from cluster.
    pub fn place(&self, _cpu: u32, _memory_mb: u64) -> Result<PlacementDecision, String> {
        Ok(PlacementDecision {
            node: "localhost".into(),
            vm_count: 0,
            cpu_available: 0,
            memory_available_mb: 0,
        })
    }
}

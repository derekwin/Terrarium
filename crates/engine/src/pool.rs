//! Warm pool management — pre-booted VM slots via CH snapshot/restore.
//!
//! Flow: template VM boots with clean overlay → CH snapshot saved.
//! Pool maintains N slots, each with a snapshot file.
//! Claim: restore snapshot with user overlay → VM ready in ~100ms.
//! Release: user overlay destroyed, slot recreated for next use.

use std::collections::HashMap;

use crate::spec::VmSpec;

/// A pool slot: a CH snapshot file waiting to be restored.
struct PoolSlot {
    /// CH snapshot path.
    snapshot_path: String,
    /// Whether this slot is currently claimed.
    claimed: bool,
}

pub struct WarmPool {
    /// Pool name → list of slots.
    slots: HashMap<String, Vec<PoolSlot>>,
    /// Pool name → VmSpec template.
    templates: HashMap<String, VmSpec>,
}

impl WarmPool {
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
            templates: HashMap::new(),
        }
    }

    /// Create a pool with N slots. Each slot has a CH snapshot + clean overlay.
    /// Caller must have already taken the CH snapshot for the template VM.
    pub fn create_pool(
        &mut self,
        name: &str,
        spec: &VmSpec,
        size: usize,
        snapshot_path: &str,
    ) -> Result<(), String> {
        if self.templates.contains_key(name) {
            return Err(format!("Pool '{}' already exists", name));
        }

        let mut pool_slots = Vec::with_capacity(size);
        for _ in 0..size {
            pool_slots.push(PoolSlot {
                snapshot_path: snapshot_path.to_string(),
                claimed: false,
            });
        }

        self.templates.insert(name.to_string(), spec.clone());
        self.slots.insert(name.to_string(), pool_slots);
        tracing::info!(pool = %name, size, "Pool created");
        Ok(())
    }

    /// Claim a slot from the pool. Returns the snapshot path to restore.
    pub fn claim(&mut self, name: &str) -> Result<String, String> {
        let slots = self
            .slots
            .get_mut(name)
            .ok_or_else(|| format!("Pool '{}' not found", name))?;

        for (i, slot) in slots.iter_mut().enumerate() {
            if !slot.claimed {
                slot.claimed = true;
                tracing::info!(pool = %name, slot = i, "Claimed");
                return Ok(slot.snapshot_path.clone());
            }
        }
        Err(format!("Pool '{}' exhausted — scale up", name))
    }

    /// Release a slot back to the pool.
    #[allow(dead_code)]
    pub fn release(&mut self, name: &str, slot_index: usize) -> Result<(), String> {
        let slots = self
            .slots
            .get_mut(name)
            .ok_or_else(|| format!("Pool '{}' not found", name))?;

        if let Some(slot) = slots.get_mut(slot_index) {
            slot.claimed = false;
            tracing::info!(pool = %name, slot = slot_index, "Released");
            Ok(())
        } else {
            Err(format!("Slot {} out of range", slot_index))
        }
    }

    /// List pools and their available/claimed slot counts.
    pub fn list(&self) -> Vec<(String, usize, usize)> {
        self.slots
            .iter()
            .map(|(name, v)| {
                let available = v.iter().filter(|s| !s.claimed).count();
                (name.clone(), available, v.len())
            })
            .collect()
    }
}

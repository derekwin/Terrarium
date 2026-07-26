//! VmManager — registry of running VMs and managed disks.
//!
//! Lifecycle separation: VM commands (spawn/shutdown/kill/destroy) only
//! control compute — they never create or delete disks. Disks (qcow2
//! overlays) have their own lifecycle via disk_create/disk_delete and
//! are attached to VMs by name at spawn time.

use std::collections::HashMap;
use std::sync::Arc;

use adapter_traits::{AdapterError, VmAdapter, VmHandle, VmName, VmSpec};

/// A managed disk (qcow2 overlay) tracked by the engine.
#[derive(Debug, Clone)]
pub struct DiskInfo {
    /// Absolute path to the overlay file.
    pub path: String,
    /// Backing (base image) path — read-only to the VMM, needed for
    /// landlock whitelisting when the disk is attached.
    pub backing: String,
    /// Virtual size ceiling in GB.
    pub size_gb: u64,
}

/// Central VM + disk registry for the controller.
pub struct VmManager {
    adapter: Arc<dyn VmAdapter>,
    vms: HashMap<VmName, Box<dyn VmHandle>>,
    /// VM name -> attached disk name (for in-use checks on disk_delete).
    vm_disks: HashMap<VmName, String>,
    /// Disk registry: disk name -> info. Rebuilt from the state dir at
    /// startup so disks survive daemon restarts (that is the point of
    /// separating their lifecycle from VMs).
    disks: HashMap<String, DiskInfo>,
}

impl VmManager {
    /// Create a new VM manager with the given adapter.
    pub fn new(adapter: Arc<dyn VmAdapter>) -> Self {
        let mut mgr = Self {
            adapter,
            vms: HashMap::new(),
            vm_disks: HashMap::new(),
            disks: HashMap::new(),
        };
        mgr.scan_disks();
        mgr
    }

    fn state_dir() -> String {
        std::env::var("TERRA_STATE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/tmp/terra-disks/vms".to_string())
    }

    /// Rebuild the disk registry by scanning the state directory.
    /// Layout: <state_dir>/<disk-name>/overlay.qcow2
    fn scan_disks(&mut self) {
        let state_dir = Self::state_dir();
        let entries = match std::fs::read_dir(&state_dir) {
            Ok(e) => e,
            Err(_) => return, // no state dir yet — nothing to scan
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let overlay = entry.path().join("overlay.qcow2");
            if !overlay.exists() {
                continue;
            }
            match read_qcow2_backing(&overlay) {
                Ok(Some(backing)) => {
                    tracing::info!(disk = %name, "Recovered managed disk from state dir");
                    self.disks.insert(
                        name,
                        DiskInfo {
                            path: overlay.to_string_lossy().into_owned(),
                            backing,
                            size_gb: 0, // unknown from scan; informational only
                        },
                    );
                }
                Ok(None) => {
                    tracing::warn!(disk = %name, "overlay has no backing file, skipping");
                }
                Err(e) => {
                    tracing::warn!(disk = %name, error = %e, "failed to read overlay, skipping");
                }
            }
        }
    }

    /// Spawn a new VM from the given spec, optionally attaching a managed
    /// disk by name. The disk must already exist (see disk_create) —
    /// spawn never creates disks.
    pub async fn spawn(&mut self, spec: VmSpec, disk: Option<&str>) -> Result<(), AdapterError> {
        let name = spec.name.clone();
        if self.vms.contains_key(&name) {
            return Err(AdapterError::internal(format!(
                "VM '{}' already exists",
                name
            )));
        }

        let mut spec = spec;
        // The adapter must not create overlays itself — disks are
        // managed exclusively by the engine registry.
        spec.base_disk = None;

        if let Some(disk_name) = disk {
            let info = self.disks.get(disk_name).ok_or_else(|| {
                AdapterError::not_found(format!(
                    "disk '{}' not found — create it first with disk_create",
                    disk_name
                ))
            })?;
            // A qcow2 overlay is a single-writer disk — refuse to attach
            // it to a second running VM (same guard as disk_delete).
            if let Some(vm) = self.disk_in_use_by(disk_name) {
                return Err(AdapterError::internal(format!(
                    "disk '{}' is already attached to VM '{}'",
                    disk_name, vm
                )));
            }
            spec.disks.push(info.path.clone());
            // The VMM opens the backing file implicitly via the overlay's
            // qcow2 header — record it so the adapter can whitelist it
            // for CH --landlock.
            spec.overlay_backing.push(info.backing.clone());
            self.vm_disks.insert(name.clone(), disk_name.to_string());
        }

        let handle = self.adapter.create(&spec).await?;
        self.vms.insert(name, handle);
        Ok(())
    }

    /// Get a reference to a running VM by name.
    pub fn get(&self, name: &str) -> Option<&dyn VmHandle> {
        self.vms.get(name).map(|v| v.as_ref())
    }

    /// List all VM names.
    pub fn list_names(&self) -> Vec<&str> {
        self.vms.keys().map(|s| s.as_ref()).collect()
    }

    /// Gracefully shut down a VM by name and remove it from the registry.
    /// The attached disk (if any) is kept.
    pub async fn shutdown(&mut self, name: &str) -> Result<(), AdapterError> {
        let handle = self
            .vms
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        self.vm_disks.remove(name);
        handle.shutdown().await
    }

    /// Force-kill a VM by removing it from the registry; the handle's
    /// Drop kills the process. The attached disk (if any) is kept.
    pub async fn kill(&mut self, name: &str) -> Result<(), AdapterError> {
        self.vms
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        self.vm_disks.remove(name);
        Ok(())
    }

    /// Destroy a VM: stop it and remove it from the registry.
    /// Never touches disks — data outlives compute by design.
    pub async fn destroy(&mut self, name: &str) -> Result<(), AdapterError> {
        let handle = self
            .vms
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;
        self.vm_disks.remove(name);
        handle.shutdown().await
    }

    /// Create a managed disk (qcow2 overlay on top of `base`).
    /// Returns the overlay path.
    pub fn disk_create(
        &mut self,
        name: &str,
        base: &str,
        size_gb: u64,
    ) -> Result<String, AdapterError> {
        // Reuse VmName's whitelist — disk names become directory names.
        let valid_name = VmName::new(name.to_string()).map_err(AdapterError::invalid_argument)?;
        if self.disks.contains_key(valid_name.as_ref()) {
            return Err(AdapterError::internal(format!(
                "disk '{}' already exists",
                name
            )));
        }
        if !std::path::Path::new(base).exists() {
            return Err(AdapterError::not_found(format!(
                "base image '{}' not found",
                base
            )));
        }
        let spec = overlay::OverlaySpec::new(name, base).disk_size_gb(size_gb);
        let path = overlay::OverlayManager::create_or_reuse(&spec)
            .map_err(|e| AdapterError::internal(format!("overlay: {}", e)))?;
        self.disks.insert(
            name.to_string(),
            DiskInfo {
                path: path.clone(),
                backing: base.to_string(),
                size_gb,
            },
        );
        Ok(path)
    }

    /// List all managed disks.
    pub fn disk_list(&self) -> Vec<(String, DiskInfo)> {
        let mut out: Vec<_> = self
            .disks
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Get info about a managed disk.
    pub fn disk_info(&self, name: &str) -> Option<DiskInfo> {
        self.disks.get(name).cloned()
    }

    /// The name of the VM currently using a disk, if any.
    pub fn disk_in_use_by(&self, name: &str) -> Option<&str> {
        self.vm_disks
            .iter()
            .find(|(_, d)| d.as_str() == name)
            .map(|(vm, _)| vm.as_ref())
    }

    /// Delete a managed disk. Refuses while a VM is using it.
    pub fn disk_delete(&mut self, name: &str) -> Result<(), AdapterError> {
        if let Some(vm) = self.disk_in_use_by(name) {
            return Err(AdapterError::internal(format!(
                "disk '{}' is in use by VM '{}' — destroy the VM first",
                name, vm
            )));
        }
        let info = self
            .disks
            .remove(name)
            .ok_or_else(|| AdapterError::not_found(format!("disk '{}' not found", name)))?;
        std::fs::remove_file(&info.path)
            .map_err(|e| AdapterError::internal(format!("remove overlay: {}", e)))?;
        // Best-effort: remove the (now empty) per-disk directory.
        if let Some(dir) = std::path::Path::new(&info.path).parent() {
            let _ = std::fs::remove_dir(dir);
        }
        Ok(())
    }

    /// Shut down all VMs and clear the registry. Disks are kept.
    pub async fn shutdown_all(&mut self) {
        let names: Vec<VmName> = self.vms.keys().cloned().collect();
        for name in names {
            if let Err(e) = self.shutdown(name.as_ref()).await {
                tracing::warn!(%name, error = %e, "Error shutting down VM");
            }
        }
    }

    /// Reap any VMs whose processes have exited unexpectedly.
    /// Returns the names of VMs that were removed. Disks are kept.
    pub fn reap_dead(&mut self) -> Vec<VmName> {
        let mut dead = Vec::new();
        let names: Vec<VmName> = self.vms.keys().cloned().collect();
        for name in names {
            let remove = {
                if let Some(handle) = self.vms.get_mut(&name) {
                    !handle.is_alive()
                } else {
                    false
                }
            };
            if remove {
                tracing::warn!(%name, "Reaping dead VM");
                self.vms.remove(&name);
                self.vm_disks.remove(&name);
                dead.push(name);
            }
        }
        dead
    }
}

/// Read the backing file of a qcow2 image via qemu-img info.
fn read_qcow2_backing(path: &std::path::Path) -> Result<Option<String>, String> {
    let output = std::process::Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(path)
        .output()
        .map_err(|e| format!("qemu-img info: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "qemu-img info: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parse qemu-img output: {}", e))?;
    Ok(v["full-backing-filename"]
        .as_str()
        .or_else(|| v["backing-filename"].as_str())
        .map(String::from))
}

impl Default for VmManager {
    fn default() -> Self {
        // Default requires an adapter. We panic if called without one —
        // callers should use VmManager::new(adapter) explicitly.
        panic!("VmManager requires an adapter; use VmManager::new()")
    }
}

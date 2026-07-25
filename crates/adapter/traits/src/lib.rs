//! Adapter trait definitions for Terrarium Engine.
//!
//! These traits decouple the engine from specific VM and sandbox
//! implementations. Each backend (CH, Firecracker, K8s Pod, sandboxd,
//! Sandlock, etc.) implements the corresponding trait.

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Common types
// ---------------------------------------------------------------------------

/// A validated VM or sandbox name. Only allows `[a-zA-Z0-9_-]+`.
/// This prevents path traversal when names are used in file paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct VmName(String);

impl VmName {
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        if name.is_empty() {
            return Err("name must not be empty".into());
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "invalid name '{}': only [a-zA-Z0-9_-] allowed",
                name
            ));
        }
        Ok(Self(name))
    }
}

impl fmt::Display for VmName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for VmName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for VmName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for VmName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        VmName::new(s).map_err(serde::de::Error::custom)
    }
}

/// Specification for creating a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub name: VmName,
    /// Path to kernel image (bzImage / vmlinux.bin).
    pub kernel: String,
    /// Kernel command line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    /// Number of vCPUs to boot with.
    pub boot_vcpus: u8,
    /// Maximum vCPUs (None = fixed, no hotplug).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u8>,
    /// Boot memory in MB.
    pub memory_mb: u64,
    /// Maximum memory in MB (None = fixed, no hotplug).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u64>,
    /// Path to initramfs (cpio archive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<String>,
    /// Disk images to attach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<String>,
    /// Base disk for qcow2 overlay (shared, read-only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_disk: Option<String>,
    /// Virtual disk size for overlay in GB.
    #[serde(default = "default_disk_size_gb")]
    pub disk_size_gb: u64,
    /// Backend-specific configuration (JSON blob, adapter-defined).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_config: Option<serde_json::Value>,
}

/// What this VM backend can and cannot do.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VmCapabilities {
    pub cpu_resize: bool,
    pub memory_resize: bool,
    pub disk_resize: bool,
    pub disk_add: bool,
    pub snapshot: bool,
    pub pause_resume: bool,
    #[serde(default)]
    pub network_qos: bool,
    /// Whether this VMM supports qcow2 backing chains (COW at block level).
    /// If false, the overlay crate falls back to raw disk conversion.
    #[serde(default = "default_true")]
    pub qcow2: bool,
}

fn default_true() -> bool {
    true
}

/// Network QoS configuration for a VM.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkQos {
    /// Egress bandwidth limit in kbps (0 = unlimited).
    pub egress_kbps: u64,
    /// Ingress bandwidth limit in kbps (0 = unlimited).
    pub ingress_kbps: u64,
    /// Priority class (0 = lowest, higher = preferred under congestion).
    pub priority: u32,
}

/// VM info returned by the adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInfo {
    pub state: String,
    pub cpus: Option<u8>,
    pub memory_mb: Option<u64>,
}

/// Snapshot data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub path: String,
}

/// Specification for creating a sandbox inside a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub name: VmName,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub limits: ResourceLimits,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub memory_mb: Option<u64>,
    pub cpu_shares: Option<u64>,
}

/// Command to execute inside a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecCommand {
    pub args: Vec<String>,
    pub work_dir: Option<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
}

/// Result of a command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

fn default_disk_size_gb() -> u64 {
    20
}

impl VmSpec {
    /// Validate the spec before passing it to an adapter.
    /// Returns Ok(()) or an error message describing the first violation.
    pub fn validate(&self) -> Result<(), String> {
        if self.boot_vcpus == 0 {
            return Err("boot_vcpus must be at least 1".into());
        }
        if let Some(max) = self.max_vcpus {
            if max == 0 {
                return Err("max_vcpus must be at least 1 (or None for fixed)".into());
            }
            if self.boot_vcpus > max {
                return Err(format!(
                    "boot_vcpus ({}) exceeds max_vcpus ({})",
                    self.boot_vcpus, max
                ));
            }
        }
        if self.memory_mb == 0 {
            return Err("memory_mb must be at least 1".into());
        }
        if let Some(max) = self.max_memory_mb {
            if max == 0 {
                return Err("max_memory_mb must be at least 1 (or None for fixed)".into());
            }
            if self.memory_mb > max {
                return Err(format!(
                    "memory_mb ({}) exceeds max_memory_mb ({})",
                    self.memory_mb, max
                ));
            }
            // hotplug_size = max_memory_mb / 1024 GB; must be at least 1G
            if max < 1024 {
                return Err(format!(
                    "max_memory_mb ({}) too small for virtio-mem hotplug (minimum 1024 MB)",
                    max
                ));
            }
        }
        if self.kernel.is_empty() {
            return Err("kernel path must not be empty".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// VmAdapter — backed by a VMM (CH, Firecracker, K8s Pod, etc.)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VmAdapter: Send + Sync {
    /// What this backend can and cannot do.
    fn capabilities(&self) -> VmCapabilities;

    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>, String>;
    async fn restore(
        &self,
        snapshot: &Snapshot,
        spec: &VmSpec,
    ) -> Result<Box<dyn VmHandle>, String>;
}

#[async_trait]
pub trait VmHandle: Send + Sync {
    async fn info(&self) -> Result<VmInfo, String>;

    /// Resize vCPUs and/or memory. Backends that don't support this
    /// return an error; the engine checks capabilities() first.
    async fn resize(&self, cpu: Option<u32>, memory: Option<u64>) -> Result<(), String>;

    /// Resize an existing disk (online expand). Not supported by all backends.
    async fn resize_disk(&self, disk_id: &str, size: u64) -> Result<(), String>;

    /// Hot-add a new disk. Not supported by all backends.
    async fn add_disk(&self, path: &str, disk_id: &str) -> Result<(), String>;

    /// Apply network QoS (rate limiting + priority). Implemented via tc on TAP.
    async fn set_network_qos(&self, qos: &NetworkQos) -> Result<(), String>;

    /// Take a VM snapshot. Not supported by all backends.
    async fn snapshot(&self) -> Result<Snapshot, String>;

    /// Pause the VM. Not supported by all backends.
    async fn pause(&self) -> Result<(), String>;

    /// Resume a paused VM.
    async fn resume(&self) -> Result<(), String>;

    async fn shutdown(&self) -> Result<(), String>;
    fn pid(&self) -> u32;
    /// Check if the VM/sandbox process is still running.
    /// Uses `try_wait()` on the underlying child process.
    fn is_alive(&mut self) -> bool;
}

// ---------------------------------------------------------------------------
// SandboxAdapter — backed by a sandbox tech (sandboxd, Sandlock, gVisor, etc.)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait SandboxAdapter: Send + Sync {
    async fn create(
        &self,
        vm: &dyn VmHandle,
        spec: &SandboxSpec,
    ) -> Result<Box<dyn SandboxHandle>, String>;
}

#[async_trait]
pub trait SandboxHandle: Send + Sync {
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, String>;
    async fn setup(&self, tools: &[String]) -> Result<(), String>;
    async fn destroy(&self) -> Result<(), String>;
}

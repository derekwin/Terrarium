//! Adapter trait definitions for Terrarium Engine.
//!
//! These traits decouple the engine from specific VM and sandbox
//! implementations. Each backend (CH, Firecracker, K8s Pod, sandboxd,
//! Sandlock, etc.) implements the corresponding trait.

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Unified error type for all adapter operations.
#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("{0}")]
    Internal(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("not supported: {0}")]
    NotSupported(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("timeout: {0}")]
    Timeout(String),
}

impl AdapterError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn not_supported(msg: impl Into<String>) -> Self {
        Self::NotSupported(msg.into())
    }
    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }
    pub fn timeout(msg: impl Into<String>) -> Self {
        Self::Timeout(msg.into())
    }

    /// True if this error is likely transient and the operation can be retried.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }
}

// Convenience: convert from &str / String for ergonomic use.
impl From<&str> for AdapterError {
    fn from(s: &str) -> Self {
        Self::Internal(s.to_string())
    }
}

impl From<String> for AdapterError {
    fn from(s: String) -> Self {
        Self::Internal(s)
    }
}

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
    /// Attach a virtio-net device backed by a host tap + NAT bridge.
    /// Guest gets DHCP from the bridge (10.200.0.x by default).
    #[serde(default)]
    pub net: bool,
    /// Layered root filesystem (virtiofs). When set, the adapter composes
    /// the named layers on the host and boots the VM with the result as
    /// its rootfs (initramfs is then only the thin virtiofs bootstrap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsSpec>,
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
    /// Whether this VMM can share a host directory tree with the guest
    /// (virtiofs or equivalent). False for block-only backends.
    #[serde(default)]
    pub virtio_fs: bool,
}

/// Writable-layer policy for a layered (virtiofs) root filesystem.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum UpperPolicy {
    /// Per-VM upperdir, deleted when the VM is destroyed (default).
    #[default]
    Ephemeral,
    /// Named upperdir that survives VM destruction and is re-attached
    /// when a later VM requests the same name.
    Persistent(String),
}

/// Layered root filesystem configuration (EROFS/plain layers composed
/// with OverlayFS on the host, exposed via virtiofs).
///
/// Layers are named directories under the engine's layer dir, given
/// bottom-to-top: the LAST entry is the base layer, earlier entries
/// override it (OverlayFS lowerdir is built right-to-left).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsSpec {
    /// Layer names, highest priority first, base layer last.
    pub layers: Vec<String>,
    /// Writable layer policy.
    #[serde(default)]
    pub upper: UpperPolicy,
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

/// Per-exec sandbox policy, applied by sandlock in the guest.
///
/// All fields are optional; an absent policy (None at the call site) keeps
/// the hardcoded guest default. Path grants are APPEND-mode: the default
/// policy (RO system dirs, RW workdir + /tmp, /dev grants) always applies
/// and user grants add on top — there is no replace mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecPolicy {
    /// Extra read-only path grants, appended to the default grants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_paths: Vec<String>,
    /// Extra read-write path grants, appended to the default grants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_paths: Vec<String>,
    /// sandlock `--net-allow` entries. Absent (None) → network
    /// unrestricted (current default); present → deny-by-default egress
    /// with these entries passed through verbatim. Must be non-empty when
    /// present — an empty list is rejected by the engine/guest validation
    /// layers (zero flags would silently leave egress unrestricted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_allow: Option<Vec<String>>,
    /// sandlock `-m <n>M` memory limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    /// sandlock `-P <n>` process-count limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procs: Option<u32>,
}

/// Options for a single [`VmHandle::exec`] call.
#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    /// Command argv (exec runs it inside the VM via the guest agent).
    pub args: Vec<String>,
    /// Per-command timeout in seconds.
    pub timeout_secs: u64,
    /// Run under sandlock (Landlock/seccomp) confinement in the guest.
    pub sandbox: bool,
    /// Guest-side working directory (None = agent default).
    pub work_dir: Option<String>,
    /// Register the process under this id in the guest so it can be
    /// killed later via `kill_exec`.
    pub exec_id: Option<String>,
    /// Sandlock policy for this exec (only meaningful with `sandbox`).
    pub policy: Option<ExecPolicy>,
}

impl ExecOpts {
    /// Minimal blocking exec: just argv and a timeout.
    pub fn new(args: Vec<String>, timeout_secs: u64) -> Self {
        Self {
            args,
            timeout_secs,
            ..Self::default()
        }
    }

    /// Builder: run under sandlock confinement in the guest.
    pub fn with_sandbox(mut self, sandbox: bool) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Builder: set the guest-side working directory.
    pub fn with_work_dir(mut self, work_dir: impl Into<String>) -> Self {
        self.work_dir = Some(work_dir.into());
        self
    }

    /// Builder: register the exec under this id (for `kill_exec`).
    pub fn with_exec_id(mut self, exec_id: impl Into<String>) -> Self {
        self.exec_id = Some(exec_id.into());
        self
    }

    /// Builder: attach a sandlock policy to this exec.
    pub fn with_policy(mut self, policy: ExecPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
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

    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>, AdapterError>;
    async fn restore(
        &self,
        snapshot: &Snapshot,
        spec: &VmSpec,
    ) -> Result<Box<dyn VmHandle>, AdapterError>;
}

#[async_trait]
pub trait VmHandle: Send + Sync {
    async fn info(&self) -> Result<VmInfo, AdapterError>;

    /// Resize vCPUs and/or memory. Backends that don't support this
    /// return an error; the engine checks capabilities() first.
    async fn resize(&self, cpu: Option<u32>, memory: Option<u64>) -> Result<(), AdapterError>;

    /// Execute a command inside the VM (via the guest agent, e.g.
    /// guest-proxy over vsock). See [`ExecOpts`] for the knobs: `sandbox`
    /// requests sandlock (Landlock/seccomp) confinement in the guest,
    /// `work_dir` sets the guest-side working directory (None = agent
    /// default), `exec_id` registers the process under that id in the
    /// guest so it can be killed later via `kill_exec`, and `policy`
    /// customizes the sandlock policy. Default: not supported.
    async fn exec(&self, _opts: &ExecOpts) -> Result<ExecResult, AdapterError> {
        Err(AdapterError::not_supported(
            "exec not supported by this backend",
        ))
    }

    /// Kill a previously `exec_id`-registered exec inside the VM
    /// (SIGKILL to its process group). Default: not supported.
    async fn kill_exec(&self, _exec_id: &str) -> Result<(), AdapterError> {
        Err(AdapterError::not_supported(
            "kill_exec not supported by this backend",
        ))
    }

    /// Hot-plug a layered filesystem into a running VM (warm-pool attach:
    /// compose layers on the host, hot-add the virtiofs device, and mount
    /// it inside the guest via the guest-proxy vsock channel).
    /// Default: not supported by this backend.
    async fn attach_fs(&self, _fs: &FsSpec) -> Result<(), AdapterError> {
        Err(AdapterError::not_supported(
            "attach_fs not supported by this backend",
        ))
    }

    /// Detach a previously attached layered filesystem (guest umount +
    /// device removal + host-side teardown).
    async fn detach_fs(&self) -> Result<(), AdapterError> {
        Err(AdapterError::not_supported(
            "detach_fs not supported by this backend",
        ))
    }

    /// Take a VM snapshot. Not supported by all backends.
    async fn snapshot(&self) -> Result<Snapshot, AdapterError>;

    /// Pause the VM. Not supported by all backends.
    async fn pause(&self) -> Result<(), AdapterError>;

    /// Resume a paused VM.
    async fn resume(&self) -> Result<(), AdapterError>;

    async fn shutdown(&self) -> Result<(), AdapterError>;
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
    ) -> Result<Box<dyn SandboxHandle>, AdapterError>;
}

#[async_trait]
pub trait SandboxHandle: Send + Sync {
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, AdapterError>;
    async fn setup(&self, tools: &[String]) -> Result<(), AdapterError>;
    async fn destroy(&self) -> Result<(), AdapterError>;
}

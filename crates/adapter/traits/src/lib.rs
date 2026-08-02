//! Adapter trait definitions for Terrarium Engine.
//!
//! These traits decouple the engine from specific VM and sandbox
//! implementations. Each backend (CH, Firecracker, K8s Pod, sandboxd,
//! Sandlock, etc.) implements the corresponding trait.

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::sync::Arc;
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
}

/// Writable-layer policy for a layered (virtiofs) root filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
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
    /// Capability-based policy for the sandbox; `None` lets the engine
    /// inject its default (deny-by-default) policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<SandboxPolicy>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub memory_mb: Option<u64>,
    pub cpu_shares: Option<u64>,
    pub procs: Option<u32>,
    pub fds: Option<u32>,
    pub bandwidth_kbps: Option<u64>,
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
    /// Capability-based sandbox policy for this exec (only meaningful with
    /// `sandbox`). See [`SandboxPolicy`].
    pub policy: Option<SandboxPolicy>,
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
}

/// Command to execute inside a sandbox.
///
/// `policy_override` is a per-call override on a bound session: it is
/// unioned onto the policy bound at [`SandboxAdapter::create`] by the
/// backend (base first, override capabilities appended — never a replace).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecCommand {
    pub args: Vec<String>,
    pub work_dir: Option<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
    /// Per-call policy override on the session's bound policy. Optional:
    /// `None` runs with exactly the bound policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_override: Option<SandboxPolicy>,
    /// Per-command timeout in seconds; `None` uses the backend's default
    /// (guest sandlock: 60s, matching the engine's exec default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
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

    /// Project this create-time spec into the design's [`VmPolicy`] — the
    /// VM-layer physical policy owned by the `VmAdapter` (policy-model.md
    /// §2.1). `VmSpec` remains the create-time implementation carrier;
    /// this is the canonical projection the engine records per VM, so the
    /// sandbox-limits-⊆-VM-quota invariant (§3.5) has a concrete quota to
    /// check against.
    ///
    /// Mapping: `net` → [`VmNetwork::Nat`] when networking is enabled,
    /// else [`VmNetwork::None`] (`VmSpec` has no bridge equivalent);
    /// storage `upper` comes from the `fs` layer spec (persistent name or
    /// ephemeral); the bandwidth field has no `VmSpec` counterpart and
    /// stays `None`.
    pub fn to_policy(&self) -> VmPolicy {
        VmPolicy {
            resources: VmResources {
                cpus: self.boot_vcpus,
                memory_mb: self.memory_mb,
                max_cpus: self.max_vcpus,
                max_memory_mb: self.max_memory_mb,
                bandwidth_kbps: None,
            },
            network: if self.net {
                VmNetwork::Nat
            } else {
                VmNetwork::None
            },
            storage: VmStorage {
                upper: self
                    .fs
                    .as_ref()
                    .map(|fs| fs.upper.clone())
                    .unwrap_or(UpperPolicy::Ephemeral),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// VmAdapter — backed by a VMM (CH, Firecracker, K8s Pod, etc.)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VmAdapter: Send + Sync {
    /// Boundary contract (L1): Confidentiality + Integrity — the VM is the
    /// physical isolation boundary between tenants (D1); Availability — the
    /// create-time physical quota (D4).
    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>, AdapterError>;
    /// Boundary contract (L1): Availability — session-lifecycle restore of
    /// a persisted state (D3).
    async fn restore(
        &self,
        snapshot: &Snapshot,
        spec: &VmSpec,
    ) -> Result<Box<dyn VmHandle>, AdapterError>;
}

#[async_trait]
pub trait VmHandle: Send + Sync {
    /// Boundary contract (L1): Availability — inspectability of VM state
    /// and resource usage (D3/D6).
    async fn info(&self) -> Result<VmInfo, AdapterError>;

    /// Boundary contract (L1): Availability — runtime resource governance
    /// (D4). Resize vCPUs and/or memory. Backends that don't support this
    /// return an error.
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

    /// Ping the guest agent (readiness probe, e.g. before slating a
    /// freshly booted pool VM as claimable). Default: not supported.
    async fn ping(&self) -> Result<(), AdapterError> {
        Err(AdapterError::not_supported(
            "ping not supported by this backend",
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

    /// Take a VM snapshot. Boundary contract (L1): Availability — state
    /// persistence for fault tolerance (D3). Not supported by all backends.
    async fn snapshot(&self) -> Result<Snapshot, AdapterError>;

    /// Pause the VM. Boundary contract (L1): Availability — resource
    /// control (D3). Default: not supported.
    async fn pause(&self) -> Result<(), AdapterError> {
        Err(AdapterError::not_supported(
            "pause not supported by this backend",
        ))
    }

    /// Resume a paused VM. Boundary contract (L1): Availability — resource
    /// control (D3). Default: not supported.
    async fn resume(&self) -> Result<(), AdapterError> {
        Err(AdapterError::not_supported(
            "resume not supported by this backend",
        ))
    }

    /// Boundary contract (L1): Availability — resource reclamation (D3/D4).
    async fn shutdown(&self) -> Result<(), AdapterError>;
    fn pid(&self) -> u32;
    /// Check if the VM/sandbox process is still running.
    /// Uses `try_wait()` on the underlying child process.
    /// `&self` (not `&mut`) so reap can probe handles shared by
    /// in-flight background exec tasks.
    fn is_alive(&self) -> bool;
}

// ---------------------------------------------------------------------------
// SandboxAdapter — backed by a sandbox tech (sandboxd, Sandlock, gVisor, etc.)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait SandboxAdapter: Send + Sync {
    /// Create a session: binds a VM plus its effective policy into a
    /// session context (D1 isolation / D7 policy — see
    /// docs/design/agent-exec-env-boundaries.md). The policy is understood
    /// host-side and translated into the backend's isolation primitives
    /// (default: shipped to the guest sandlock via vsock). This is the
    /// L2 reference-monitor entry (Complete Mediation): every command
    /// executed on the returned handle runs inside this bound context.
    ///
    /// Boundary contract (L2): Confidentiality + Integrity (D1 isolation),
    /// Non-interference (D1 — sibling sessions unreachable), Availability
    /// (D4 — the resource limits bound here must stay within the VM quota).
    ///
    /// The adapter receives an owned `Arc<dyn VmHandle>` because a session
    /// backend needs a live reference to its execution substrate at
    /// exec() time — a `&dyn` borrow cannot outlive `create`.
    async fn create(
        &self,
        vm: Arc<dyn VmHandle>,
        spec: &SandboxSpec,
    ) -> Result<Box<dyn SandboxHandle>, AdapterError>;
}

#[async_trait]
pub trait SandboxHandle: Send + Sync {
    /// Boundary contract (L2): Confidentiality + Integrity — complete
    /// mediation of every command inside the bound context (D1/D7);
    /// Availability — per-call resource bounds (D4).
    /// Run a command within the bound session context. The policy is fixed
    /// at create time; a per-call `policy_override` on the command is
    /// unioned onto the bound policy by the backend (never a replace).
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, AdapterError>;
    /// Boundary contract (L2): Integrity — session tooling installed
    /// inside the bound context (D2).
    /// Install persistent tools/state in the session, if the backend has
    /// any (no-op for per-exec confinement backends like guest sandlock).
    async fn setup(&self, tools: &[String]) -> Result<(), AdapterError>;
    /// Boundary contract (L2): Availability — session resource reclamation
    /// (D3/D4). Tear down the session and release any backend resources.
    async fn destroy(&self) -> Result<(), AdapterError>;
}

// ---------------------------------------------------------------------------
// Policy model (B1) — two-layer policy: VmPolicy (physical, VmAdapter) and
// SandboxPolicy (logical capabilities, SandboxAdapter). See
// docs/design/policy-model.md. Capability-based: default deny, explicit grant.
// ---------------------------------------------------------------------------

/// Path reference: exact or prefix match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathPattern {
    Exact(std::path::PathBuf),
    Prefix(std::path::PathBuf),
}

/// File access at the least-privilege granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileAccess {
    Read,
    ReadWrite,
    Execute,
}

/// Network endpoint (host:port; omitted port = any).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Outbound,
    Inbound,
}

/// A single access grant held by a subject (capability).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    File {
        path: PathPattern,
        access: FileAccess,
    },
    Network {
        endpoint: Endpoint,
        direction: Direction,
    },
    Device {
        path: std::path::PathBuf,
    },
}

/// Explicit default access — avoids implementation drift (guest-side
/// hardcoding). Deny is the fail-safe default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefaultAccess {
    #[default]
    #[serde(rename = "deny")]
    Deny,
    #[serde(rename = "allow")]
    Allow,
}

/// Which events to audit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditSpec {
    #[serde(default)]
    pub deny: bool,
    #[serde(default)]
    pub exec: bool,
    #[serde(default)]
    pub resource: bool,
}

/// Sandbox-level policy: logical capability set + resource limits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicy {
    /// Explicit grants; an empty set (or an omitted key on the wire) is
    /// valid default-deny.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub limits: ResourceLimits,
    #[serde(default)]
    pub default: DefaultAccess,
    #[serde(default)]
    pub audit: AuditSpec,
    #[serde(default)]
    pub version: u32,
}

impl SandboxPolicy {
    /// True when the capability set grants `path` access at or above
    /// `need` (prefix patterns cover their subtrees).
    pub fn grants_path(&self, path: &std::path::Path, need: FileAccess) -> bool {
        self.capabilities.iter().any(|c| match c {
            Capability::File { path: p, access } => {
                access_ge(*access, need) && pattern_matches(p, path)
            }
            _ => false,
        })
    }

    /// Validate the capability set: every file/device path must be
    /// absolute, network endpoints must have a non-empty host and a
    /// positive port when present, resource limits must be > 0 when set,
    /// and `DefaultAccess::Allow` is rejected (debug escape hatch only).
    /// An empty capability set is valid (default-deny).
    pub fn validate(&self) -> Result<(), String> {
        for cap in &self.capabilities {
            match cap {
                Capability::File { path, access: _ } => {
                    if !is_absolute_path(path) {
                        return Err(format!(
                            "capability file path must be absolute (got '{}')",
                            path_display(path)
                        ));
                    }
                }
                Capability::Network { endpoint, .. } => {
                    if endpoint.host.is_empty() {
                        return Err("network endpoint host must not be empty".into());
                    }
                    if let Some(port) = endpoint.port {
                        if port == 0 {
                            return Err("network endpoint port must be > 0".into());
                        }
                    }
                }
                Capability::Device { path } => {
                    if !path.starts_with(std::path::Path::new("/")) {
                        return Err(format!(
                            "device path must be absolute (got '{}')",
                            path.display()
                        ));
                    }
                }
            }
        }
        if self.default == DefaultAccess::Allow {
            return Err(
                "DefaultAccess::Allow is a debug escape hatch and not allowed in production".into(),
            );
        }
        if let Some(m) = self.limits.memory_mb {
            if m == 0 {
                return Err("limits.memory_mb must be > 0".into());
            }
        }
        if let Some(p) = self.limits.procs {
            if p == 0 {
                return Err("limits.procs must be > 0".into());
            }
        }
        if let Some(f) = self.limits.fds {
            if f == 0 {
                return Err("limits.fds must be > 0".into());
            }
        }
        if let Some(b) = self.limits.bandwidth_kbps {
            if b == 0 {
                return Err("limits.bandwidth_kbps must be > 0".into());
            }
        }
        Ok(())
    }

    /// Resource limits must not exceed the enclosing VM's physical quota.
    pub fn validate_with_vm(&self, vm: &VmPolicy) -> Result<(), String> {
        if let Some(mem) = self.limits.memory_mb {
            if mem > vm.resources.memory_mb {
                return Err(format!(
                    "sandbox memory limit {} MB exceeds VM quota {} MB",
                    mem, vm.resources.memory_mb
                ));
            }
        }
        Ok(())
    }

    /// Combine this policy (the base layer) with `other` (the user layer):
    /// the effective capabilities are the UNION (base first, `other`
    /// appended, deduplicated) so a user granting only their task's paths
    /// still gets the base read-only system set. Limits: `other`'s values
    /// win when present, else this policy's (per-field: memory_mb, procs,
    /// fds, bandwidth_kbps, cpu_shares). `default` and `version` follow
    /// `other` when present, else this policy's; `audit` is the OR of both
    /// per flag.
    ///
    /// This is the base∪user merge: the engine injects its default policy
    /// as the base layer under a user's sandbox policy
    /// (`default_sandbox_policy().merged_with(&user)`), and backends union
    /// a per-call `policy_override` onto the policy bound at `create`
    /// (base first, override capabilities appended — never a replace).
    pub fn merged_with(&self, other: &SandboxPolicy) -> SandboxPolicy {
        let mut capabilities = self.capabilities.clone();
        for cap in &other.capabilities {
            if !capabilities.contains(cap) {
                capabilities.push(cap.clone());
            }
        }
        SandboxPolicy {
            capabilities,
            limits: ResourceLimits {
                memory_mb: other.limits.memory_mb.or(self.limits.memory_mb),
                procs: other.limits.procs.or(self.limits.procs),
                fds: other.limits.fds.or(self.limits.fds),
                bandwidth_kbps: other.limits.bandwidth_kbps.or(self.limits.bandwidth_kbps),
                cpu_shares: other.limits.cpu_shares.or(self.limits.cpu_shares),
            },
            default: other.default,
            audit: AuditSpec {
                deny: other.audit.deny || self.audit.deny,
                exec: other.audit.exec || self.audit.exec,
                resource: other.audit.resource || self.audit.resource,
            },
            version: if other.version != 0 {
                other.version
            } else {
                self.version
            },
        }
    }
}

fn is_absolute_path(pattern: &PathPattern) -> bool {
    let path = match pattern {
        PathPattern::Exact(p) | PathPattern::Prefix(p) => p,
    };
    path.starts_with(std::path::Path::new("/"))
}

fn path_display(pattern: &PathPattern) -> String {
    match pattern {
        PathPattern::Exact(p) | PathPattern::Prefix(p) => p.display().to_string(),
    }
}

fn access_ge(have: FileAccess, need: FileAccess) -> bool {
    use FileAccess::*;
    matches!(
        (have, need),
        (ReadWrite, _) | (Execute, Execute) | (Read, Read)
    )
}

fn pattern_matches(p: &PathPattern, path: &std::path::Path) -> bool {
    match p {
        PathPattern::Exact(e) => e == path,
        PathPattern::Prefix(prefix) => path.starts_with(prefix),
    }
}

/// VM-level physical resources (the sandbox limits' upper bound).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmResources {
    pub cpus: u8,
    pub memory_mb: u64,
    pub max_cpus: Option<u8>,
    pub max_memory_mb: Option<u64>,
    pub bandwidth_kbps: Option<u64>,
}

/// VM network topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmNetwork {
    None,
    Nat,
    Bridge { iface: String },
}

/// VM-level policy: physical resources + topology. Owned by VmAdapter;
/// `VmSpec` remains the create-time implementation carrier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmPolicy {
    pub resources: VmResources,
    pub network: VmNetwork,
    pub storage: VmStorage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmStorage {
    pub upper: UpperPolicy,
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn base_policy() -> SandboxPolicy {
        SandboxPolicy {
            capabilities: vec![
                Capability::File {
                    path: PathPattern::Prefix("/usr".into()),
                    access: FileAccess::Read,
                },
                Capability::File {
                    path: PathPattern::Prefix("/tmp".into()),
                    access: FileAccess::ReadWrite,
                },
            ],
            limits: ResourceLimits {
                memory_mb: Some(256),
                ..Default::default()
            },
            default: DefaultAccess::Deny,
            audit: AuditSpec {
                deny: true,
                ..Default::default()
            },
            version: 1,
        }
    }

    fn vm_policy() -> VmPolicy {
        VmPolicy {
            resources: VmResources {
                cpus: 2,
                memory_mb: 1024,
                max_cpus: Some(4),
                max_memory_mb: Some(2048),
                bandwidth_kbps: None,
            },
            network: VmNetwork::Nat,
            storage: VmStorage {
                upper: UpperPolicy::Ephemeral,
            },
        }
    }

    #[test]
    fn policy_roundtrip_survives_serde() {
        let p = base_policy();
        let json = serde_json::to_string(&p).unwrap();
        let back: SandboxPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn default_access_defaults_to_deny() {
        // An omitted `default` must deserialize to Deny (fail-safe).
        let json = r#"{"capabilities":[],"limits":{}}"#;
        let p: SandboxPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(p.default, DefaultAccess::Deny);
        assert_eq!(p.version, 0);
    }

    #[test]
    fn grants_path_honors_prefix_and_access() {
        let p = base_policy();
        assert!(p.grants_path(std::path::Path::new("/usr/bin/python3"), FileAccess::Read));
        assert!(!p.grants_path(
            std::path::Path::new("/usr/bin/python3"),
            FileAccess::ReadWrite
        ));
        assert!(!p.grants_path(std::path::Path::new("/etc/passwd"), FileAccess::Read));
        assert!(p.grants_path(std::path::Path::new("/tmp/x"), FileAccess::ReadWrite));
    }

    #[test]
    fn empty_capability_set_grants_nothing() {
        let p = SandboxPolicy {
            capabilities: vec![],
            ..base_policy()
        };
        assert!(!p.grants_path(std::path::Path::new("/usr"), FileAccess::Read));
        assert!(!p.grants_path(std::path::Path::new("/tmp"), FileAccess::ReadWrite));
    }

    #[test]
    fn sandbox_limits_cannot_exceed_vm_quota() {
        let vm = vm_policy();
        let ok = base_policy(); // 256 MB <= 1024 MB
        assert!(ok.validate_with_vm(&vm).is_ok());
        let over = SandboxPolicy {
            limits: ResourceLimits {
                memory_mb: Some(2048),
                ..Default::default()
            },
            ..base_policy()
        };
        let err = over.validate_with_vm(&vm).unwrap_err();
        assert!(err.contains("exceeds VM quota"), "{err}");
    }

    #[test]
    fn vm_spec_to_policy_projects_quota_and_topology() {
        let spec = VmSpec {
            name: VmName::new("test-vm").unwrap(),
            kernel: "/fake/vmlinux".into(),
            cmdline: None,
            boot_vcpus: 2,
            max_vcpus: Some(4),
            memory_mb: 1024,
            max_memory_mb: Some(2048),
            initramfs: None,
            net: true,
            fs: Some(FsSpec {
                layers: vec!["base".into()],
                upper: UpperPolicy::Persistent("work".into()),
            }),
        };
        let policy = spec.to_policy();
        assert_eq!(
            policy.resources,
            VmResources {
                cpus: 2,
                memory_mb: 1024,
                max_cpus: Some(4),
                max_memory_mb: Some(2048),
                bandwidth_kbps: None,
            }
        );
        assert_eq!(policy.network, VmNetwork::Nat);
        assert_eq!(policy.storage.upper, UpperPolicy::Persistent("work".into()));
    }

    #[test]
    fn vm_spec_to_policy_defaults_no_net_ephemeral_upper() {
        let spec = VmSpec {
            name: VmName::new("test-vm").unwrap(),
            kernel: "/fake/vmlinux".into(),
            cmdline: None,
            boot_vcpus: 1,
            max_vcpus: None,
            memory_mb: 256,
            max_memory_mb: None,
            initramfs: None,
            net: false,
            fs: None,
        };
        let policy = spec.to_policy();
        assert_eq!(policy.network, VmNetwork::None);
        assert_eq!(policy.storage.upper, UpperPolicy::Ephemeral);
        assert_eq!(policy.resources.max_cpus, None);
        assert_eq!(policy.resources.max_memory_mb, None);
    }

    #[test]
    fn merged_with_unions_caps_and_user_limits_win() {
        let base = base_policy(); // /usr read, /tmp RW, mem 256MB
        let user = SandboxPolicy {
            capabilities: vec![Capability::File {
                path: PathPattern::Prefix("/opt".into()),
                access: FileAccess::ReadWrite,
            }],
            limits: ResourceLimits {
                memory_mb: Some(512),
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = base.merged_with(&user);
        // Base capabilities are preserved (default policy as base layer).
        assert!(merged.grants_path(std::path::Path::new("/usr/bin/ls"), FileAccess::Read));
        assert!(merged.grants_path(std::path::Path::new("/tmp/x"), FileAccess::ReadWrite));
        // User capabilities are appended (union, not replace).
        assert!(merged.grants_path(std::path::Path::new("/opt/x"), FileAccess::ReadWrite));
        // User limits win.
        assert_eq!(merged.limits.memory_mb, Some(512));
        assert!(merged.capabilities.len() >= base.capabilities.len());
    }
}

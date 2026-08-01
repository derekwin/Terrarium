//! Shared protocol types for Terrarium Engine.
//!
//! This crate defines the single source of truth for the JSON command/response
//! protocol used between the engine daemon, CLI, MCP server, and Python SDK.
//!
//! All clients build `Command` structs and serialize to JSON. The engine
//! deserializes `Command` and responds with `Response`.

use serde::{Deserialize, Serialize};

// The single definition of the sandbox policy lives in adapter-traits (it is
// part of the VmHandle::exec contract); re-exported here so protocol
// clients and the engine share one type instead of two divergent ones.
pub use adapter_traits::SandboxPolicy;

/// A command sent from a client to the engine daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub command: String,

    // create / info / resize / shutdown / kill
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    // create
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    // create: add-on (tool) layer names, highest priority first.
    // The system base is appended automatically (see `system`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,

    // create: system base layer name (default "base"). Appended as the
    // bottom lowerdir when `layers` doesn't already end with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,

    // exec: command argv (exec runs it inside the VM via the guest agent)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    // create: persistent upperdir name for the layered fs (user data
    // survives VM destruction; default is ephemeral per-VM)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper: Option<String>,

    // create: attach virtio-net (tap + host NAT; guest uses DHCP)
    #[serde(default)]
    pub net: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u64>,

    // snapshot / restore
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_path: Option<String>,

    // resize
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,

    // pool_create: number of idle VMs to maintain
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_size: Option<u32>,

    // exec: per-command timeout in seconds (default 60, capped at 3600)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,

    // exec: execution mode ("blocking" or "background"; default "blocking")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_mode: Option<String>,

    // exec: run the command under sandlock (Landlock/seccomp) inside the
    // guest (default false; hard error if the image lacks sandlock)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,

    // exec / sandbox_create / sandbox_exec: per-exec sandbox policy
    // (capability-based, default deny). Only valid for sandboxed exec;
    // sandbox_create stores it so later sandbox_exec calls inherit it
    // (a per-call policy overrides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<SandboxPolicy>,

    // session commands: session_id for session_status / session_kill
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    // sandbox_create / sandbox_list / tenant_destroy: tenant name
    // (validated like VmName; the tenant VM is "tenant-<tenant>")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    // sandbox_exec / sandbox_info / sandbox_kill: sandbox id (sb-<hex>)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    // sandbox_create: allow claiming the tenant VM from the warm pool
    // (default true; false forces a cold-booted dedicated VM)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<bool>,
}

impl Command {
    /// Create a simple command with just a command name.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            name: None,
            kernel: None,
            initramfs: None,
            layers: Vec::new(),
            system: None,
            args: Vec::new(),
            upper: None,
            net: false,
            cmdline: None,
            cpus: None,
            max_cpus: None,
            memory_mb: None,
            max_memory_mb: None,
            snapshot_path: None,
            memory_bytes: None,
            pool_size: None,
            timeout_secs: None,
            exec_mode: None,
            session_id: None,
            sandbox: None,
            policy: None,
            tenant: None,
            id: None,
            pool: None,
        }
    }

    /// Create a "create VM" command.
    pub fn create(name: impl Into<String>, kernel: impl Into<String>) -> Self {
        Self {
            command: "create".into(),
            name: Some(name.into()),
            kernel: Some(kernel.into()),
            ..Self::new("")
        }
    }

    /// Builder: set command name.
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = command.into();
        self
    }

    /// Builder: set VM name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Builder: set kernel path.
    pub fn with_kernel(mut self, path: impl Into<String>) -> Self {
        self.kernel = Some(path.into());
        self
    }

    /// Builder: set vCPUs.
    pub fn with_cpus(mut self, cpus: u8) -> Self {
        self.cpus = Some(cpus);
        self
    }

    /// Builder: set max vCPUs.
    pub fn with_max_cpus(mut self, max: u8) -> Self {
        self.max_cpus = Some(max);
        self
    }

    /// Set the maximum memory in MB (enables memory hotplug up to this size).
    pub fn with_max_memory_mb(mut self, max: u64) -> Self {
        self.max_memory_mb = Some(max);
        self
    }

    /// Builder: set memory in MB.
    pub fn with_memory_mb(mut self, mb: u64) -> Self {
        self.memory_mb = Some(mb);
        self
    }

    /// Builder: set memory resize bytes.
    pub fn with_memory_bytes(mut self, bytes: u64) -> Self {
        self.memory_bytes = Some(bytes);
        self
    }

    /// Builder: set the system base layer (default "base").
    pub fn with_system(mut self, name: impl Into<String>) -> Self {
        self.system = Some(name.into());
        self
    }

    /// Builder: set virtiofs layers (highest priority first, base last).
    pub fn with_layers(mut self, layers: Vec<String>) -> Self {
        self.layers = layers;
        self
    }

    /// Builder: set exec argv.
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Builder: set persistent upperdir name.
    pub fn with_upper(mut self, name: impl Into<String>) -> Self {
        self.upper = Some(name.into());
        self
    }

    /// Builder: enable virtio-net (tap + NAT).
    pub fn with_net(mut self, net: bool) -> Self {
        self.net = net;
        self
    }

    /// Builder: set initramfs.
    pub fn with_initramfs(mut self, path: impl Into<String>) -> Self {
        self.initramfs = Some(path.into());
        self
    }

    /// Builder: set exec timeout in seconds.
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// Builder: set exec mode ("blocking" or "background").
    pub fn with_exec_mode(mut self, mode: &str) -> Self {
        self.exec_mode = Some(mode.to_string());
        self
    }

    /// Builder: run the exec under sandlock confinement in the guest.
    pub fn with_sandbox(mut self, sandbox: bool) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Builder: set the per-exec sandbox policy (capability-based).
    pub fn with_policy(mut self, policy: SandboxPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Builder: set session ID (for session_status / session_kill).
    pub fn with_session_id(mut self, id: &str) -> Self {
        self.session_id = Some(id.to_string());
        self
    }

    /// Builder: set tenant (for sandbox_create / sandbox_list / tenant_destroy).
    pub fn with_tenant(mut self, tenant: &str) -> Self {
        self.tenant = Some(tenant.to_string());
        self
    }

    /// Builder: set sandbox id (for sandbox_exec / sandbox_info / sandbox_kill).
    pub fn with_id(mut self, id: &str) -> Self {
        self.id = Some(id.to_string());
        self
    }

    /// Builder: allow/forbid claiming the tenant VM from the warm pool
    /// (sandbox_create; default allow).
    pub fn with_pool(mut self, pool: bool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Builder: set pool size.
    pub fn with_pool_size(mut self, size: u32) -> Self {
        self.pool_size = Some(size);
        self
    }

    /// Builder: set snapshot path.
    pub fn with_snapshot_path(mut self, path: impl Into<String>) -> Self {
        self.snapshot_path = Some(path.into());
        self
    }
}

/// Response sent from the engine daemon back to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            status: "ok".into(),
            data: Some(data),
            error: None,
        }
    }

    pub fn ok_msg(msg: &str) -> Self {
        Self::ok(serde_json::json!({"message": msg}))
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self {
            status: "error".into(),
            data: None,
            error: Some(error.into()),
        }
    }

    /// True if the response indicates success.
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adapter_traits::{
        AuditSpec, Capability, DefaultAccess, Direction, Endpoint, FileAccess, PathPattern,
        ResourceLimits,
    };

    /// Command with a full SandboxPolicy survives a JSON roundtrip unchanged.
    #[test]
    fn policy_roundtrip() {
        let policy = SandboxPolicy {
            capabilities: vec![
                Capability::File {
                    path: PathPattern::Prefix("/opt/data".into()),
                    access: FileAccess::Read,
                },
                Capability::Network {
                    endpoint: Endpoint {
                        host: "api.openai.com".into(),
                        port: Some(443),
                    },
                    direction: Direction::Outbound,
                },
            ],
            limits: ResourceLimits {
                memory_mb: Some(512),
                ..Default::default()
            },
            default: DefaultAccess::Deny,
            audit: AuditSpec {
                deny: true,
                ..Default::default()
            },
            version: 1,
        };
        let cmd = Command::new("sandbox_exec")
            .with_id("sb-1234abcd")
            .with_args(vec!["echo".into(), "hi".into()])
            .with_sandbox(true)
            .with_policy(policy.clone());
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(back.policy, Some(policy));
    }

    /// Absent policy → None, and the key is omitted on serialization.
    #[test]
    fn absent_policy_stays_none_and_is_omitted() {
        let cmd: Command = serde_json::from_str(r#"{"command":"exec"}"#).unwrap();
        assert!(cmd.policy.is_none());
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(!json.contains("policy"), "None policy must be omitted");
    }

    /// Omitted `default` inside the policy deserializes to Deny (fail-safe).
    #[test]
    fn default_deny_when_omitted() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"exec","policy":{"capabilities":[]}}"#).unwrap();
        assert_eq!(cmd.policy.as_ref().unwrap().default, DefaultAccess::Deny);
    }

    /// B2: a limits-only policy (no `capabilities` key — the SDK emits
    /// exactly this for `--memory-mb/--procs` alone) must deserialize;
    /// the missing capability set defaults to empty (default-deny).
    #[test]
    fn limits_only_policy_defaults_capabilities_to_empty() {
        let cmd: Command =
            serde_json::from_str(r#"{"command":"exec","policy":{"limits":{"memory_mb":256}}}"#)
                .unwrap();
        let p = cmd.policy.as_ref().expect("policy present");
        assert!(
            p.capabilities.is_empty(),
            "missing capabilities must default to the empty set"
        );
        assert_eq!(p.limits.memory_mb, Some(256));
    }

    /// B2: a partial `audit` dict (only `deny`) must deserialize; the
    /// omitted exec/resource flags default to false.
    #[test]
    fn partial_audit_defaults_missing_flags() {
        let cmd: Command = serde_json::from_str(
            r#"{"command":"exec","policy":{"capabilities":[],"audit":{"deny":true}}}"#,
        )
        .unwrap();
        let p = cmd.policy.as_ref().expect("policy present");
        assert!(p.audit.deny);
        assert!(!p.audit.exec);
        assert!(!p.audit.resource);
    }

    /// Unknown fields inside the policy object are rejected
    /// (deny_unknown_fields on SandboxPolicy).
    #[test]
    fn policy_rejects_unknown_fields() {
        let res: Result<Command, _> =
            serde_json::from_str(r#"{"command":"exec","policy":{"bogus":1}}"#);
        assert!(res.is_err(), "unknown policy field must be rejected");
    }

    /// Unknown top-level fields are still rejected with a policy present.
    #[test]
    fn command_rejects_unknown_fields() {
        let res: Result<Command, _> =
            serde_json::from_str(r#"{"command":"exec","policy":{},"bogus":1}"#);
        assert!(res.is_err(), "unknown command field must be rejected");
    }
}

//! Sandbox policy defaults (B1).
//!
//! The engine is the policy authority: the default capability set lives
//! here (self-contained), so the same `SandboxPolicy` produces identical
//! access semantics across backends — no guest-side hardcoding drift.
//! Mirrors the guest sandlock defaults (read-only system dirs, RW /tmp
//! and /dev/null); the session workdir is dynamic and injected per
//! sandbox at creation.
//!
//! Governance default: deny events are audited by default. "Control the
//! agent" starts with seeing what it tried to do; a sandbox that does not
//! ask for audit explicitly still records denials.

use adapter_traits::{
    Capability, DefaultAccess, FileAccess, PathPattern, ResourceLimits, SandboxPolicy,
};

/// The default sandbox policy: read-only system dirs, RW `/tmp` and
/// `/dev/null`, everything else denied; denials are audited.
pub fn default_sandbox_policy() -> SandboxPolicy {
    SandboxPolicy {
        capabilities: vec![
            Capability::File {
                path: PathPattern::Prefix("/usr".into()),
                access: FileAccess::Read,
            },
            Capability::File {
                path: PathPattern::Prefix("/lib".into()),
                access: FileAccess::Read,
            },
            Capability::File {
                path: PathPattern::Prefix("/lib64".into()),
                access: FileAccess::Read,
            },
            Capability::File {
                path: PathPattern::Prefix("/bin".into()),
                access: FileAccess::Read,
            },
            Capability::File {
                path: PathPattern::Prefix("/sbin".into()),
                access: FileAccess::Read,
            },
            Capability::File {
                path: PathPattern::Prefix("/etc".into()),
                access: FileAccess::Read,
            },
            Capability::File {
                path: PathPattern::Prefix("/tmp".into()),
                access: FileAccess::ReadWrite,
            },
            Capability::File {
                path: PathPattern::Exact("/dev/null".into()),
                access: FileAccess::ReadWrite,
            },
            Capability::File {
                path: PathPattern::Exact("/dev/urandom".into()),
                access: FileAccess::Read,
            },
        ],
        limits: ResourceLimits::default(),
        default: DefaultAccess::Deny,
        audit: adapter_traits::AuditSpec {
            deny: true,
            exec: false,
            resource: false,
        },
        version: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_reads_system_writes_tmp() {
        let p = default_sandbox_policy();
        assert!(p.grants_path(std::path::Path::new("/usr/bin/ls"), FileAccess::Read));
        assert!(p.grants_path(std::path::Path::new("/etc/passwd"), FileAccess::Read));
        assert!(p.grants_path(std::path::Path::new("/tmp/x"), FileAccess::ReadWrite));
        assert!(!p.grants_path(std::path::Path::new("/etc/passwd"), FileAccess::ReadWrite));
        assert!(!p.grants_path(std::path::Path::new("/"), FileAccess::ReadWrite));
        assert_eq!(p.default, DefaultAccess::Deny);
    }
}

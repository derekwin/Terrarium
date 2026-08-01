//! Sandbox policy defaults (B1).
//!
//! The engine is the policy authority: the default capability set lives
//! here (self-contained), so the same `SandboxPolicy` produces identical
//! access semantics across backends — no guest-side hardcoding drift.
//! Mirrors the guest sandlock defaults (read-only system dirs, RW /tmp
//! and /dev/null); the session workdir is dynamic and injected per
//! sandbox at creation.

use adapter_traits::{
    AuditSpec, Capability, DefaultAccess, FileAccess, PathPattern, ResourceLimits, SandboxPolicy,
};

/// The default sandbox policy: read-only system dirs, RW `/tmp` and
/// `/dev/null`, everything else denied.
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
        audit: Default::default(),
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

/// Combine the engine default (base layer) with a user policy: the
/// effective capabilities are the UNION (base first, user appended,
/// deduplicated) so a user granting only their task's paths still gets
/// the base read-only system set (and can run /bin/sh). Limits: the
/// user's values win when present, else the base's. default/audit/version
/// follow the user when present, else the base.
pub fn merge_policies(base: SandboxPolicy, user: SandboxPolicy) -> SandboxPolicy {
    let mut capabilities = base.capabilities;
    for cap in user.capabilities {
        if !capabilities.contains(&cap) {
            capabilities.push(cap);
        }
    }
    SandboxPolicy {
        capabilities,
        limits: ResourceLimits {
            memory_mb: user.limits.memory_mb.or(base.limits.memory_mb),
            procs: user.limits.procs.or(base.limits.procs),
            fds: user.limits.fds.or(base.limits.fds),
            bandwidth_kbps: user.limits.bandwidth_kbps.or(base.limits.bandwidth_kbps),
            cpu_shares: user.limits.cpu_shares.or(base.limits.cpu_shares),
        },
        default: user.default,
        audit: AuditSpec {
            deny: user.audit.deny || base.audit.deny,
            exec: user.audit.exec || base.audit.exec,
            resource: user.audit.resource || base.audit.resource,
        },
        version: if user.version != 0 {
            user.version
        } else {
            base.version
        },
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn merge_unions_capabilities_and_keeps_base() {
        let base = default_sandbox_policy();
        let user = SandboxPolicy {
            capabilities: vec![Capability::File {
                path: PathPattern::Prefix("/opt".into()),
                access: FileAccess::ReadWrite,
            }],
            ..Default::default()
        };
        let merged = merge_policies(base.clone(), user);
        assert!(merged.grants_path(std::path::Path::new("/opt/x"), FileAccess::ReadWrite));
        // base read-only system set is preserved
        assert!(merged.grants_path(std::path::Path::new("/usr/bin/ls"), FileAccess::Read));
        assert!(merged.capabilities.len() >= base.capabilities.len());
    }

    #[test]
    fn merge_user_limits_win() {
        let base = default_sandbox_policy();
        let user = SandboxPolicy {
            limits: ResourceLimits {
                memory_mb: Some(512),
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = merge_policies(base, user);
        assert_eq!(merged.limits.memory_mb, Some(512));
    }
}

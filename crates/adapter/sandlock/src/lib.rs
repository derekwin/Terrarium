//! Default L2 backend: guest-side sandlock confinement, exposed through
//! the [`SandboxAdapter`] contract.
//!
//! `create` binds a VM plus its effective policy into a session; `exec`
//! runs the command via [`VmHandle::exec`] with `sandbox: true` and the
//! bound policy (a per-call `policy_override` is unioned on top via
//! [`SandboxPolicy::merged_with`]). The guest-proxy sandlock path is the
//! transport — this crate is a pure wrapper around it.

use std::sync::Arc;

use adapter_traits::{
    AdapterError, ExecCommand, ExecOpts, ExecResult, SandboxAdapter, SandboxHandle, SandboxPolicy,
    SandboxSpec, VmHandle,
};
use async_trait::async_trait;

/// Default sandbox backend: wraps the engine's guest-sandlock exec path.
/// Stateless — each `create` produces a bound [`SandboxHandle`].
#[derive(Default)]
pub struct GuestSandlockAdapter;

impl GuestSandlockAdapter {
    pub fn new() -> Self {
        Self
    }
}

/// Bound session: the VM handle plus the effective policy fixed at
/// `create`. Exec runs within this context; a per-call override is merged
/// onto `policy` (base first, override capabilities appended).
struct GuestSandlockHandle {
    vm: Arc<dyn VmHandle>,
    policy: SandboxPolicy,
}

#[async_trait]
impl SandboxAdapter for GuestSandlockAdapter {
    async fn create(
        &self,
        vm: Arc<dyn VmHandle>,
        spec: &SandboxSpec,
    ) -> Result<Box<dyn SandboxHandle>, AdapterError> {
        // A confinement backend cannot enforce without a complete policy.
        // The engine injects its default before create; `None` here is a
        // caller bug, surfaced honestly instead of silently running an
        // unconfined sandboxed exec.
        let policy = spec.policy.clone().ok_or_else(|| {
            AdapterError::invalid_argument(
                "GuestSandlockAdapter::create requires spec.policy (effective policy)",
            )
        })?;
        // The engine validates before create; re-validate defensively so a
        // session is never bound to an invalid policy.
        if let Err(err) = policy.validate() {
            return Err(AdapterError::invalid_argument(format!(
                "invalid sandbox policy: {}",
                err
            )));
        }
        Ok(Box::new(GuestSandlockHandle { vm, policy }))
    }
}

#[async_trait]
impl SandboxHandle for GuestSandlockHandle {
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, AdapterError> {
        // Effective policy = bound policy ∪ per-call override (base-union-
        // user merge: bound capabilities preserved, override capabilities
        // appended, override limits win).
        let policy = match &cmd.policy_override {
            Some(override_policy) => self.policy.merged_with(override_policy),
            None => self.policy.clone(),
        };

        // Per-command timeout: `None` uses the 60s default (matching the
        // engine's exec default).
        let mut opts =
            ExecOpts::new(cmd.args.clone(), cmd.timeout_secs.unwrap_or(60)).with_sandbox(true);
        if let Some(work_dir) = &cmd.work_dir {
            opts = opts.with_work_dir(work_dir.clone());
        }
        opts.policy = Some(policy);
        self.vm.exec(&opts).await
    }

    async fn destroy(&self) -> Result<(), AdapterError> {
        // Nothing to release: the guest-proxy applies rules per-run.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adapter_traits::{
        Capability, DefaultAccess, FileAccess, PathPattern, ResourceLimits, Snapshot, VmInfo,
        VmName,
    };
    use std::sync::Mutex;

    /// One recorded exec invocation (assertions on the backend → VmHandle
    /// plumbing).
    #[derive(Debug, Clone)]
    struct ExecCall {
        args: Vec<String>,
        timeout_secs: u64,
        sandbox: bool,
        work_dir: Option<String>,
        exec_id: Option<String>,
        policy: Option<SandboxPolicy>,
    }

    /// Minimal mock VmHandle: records every exec and returns a canned
    /// result. Non-exec methods use the trait defaults (not supported).
    struct MockVmHandle {
        exec_log: Arc<Mutex<Vec<ExecCall>>>,
        stdout: String,
        exit_code: i32,
    }

    impl MockVmHandle {
        fn new() -> Self {
            Self {
                exec_log: Arc::new(Mutex::new(Vec::new())),
                stdout: String::new(),
                exit_code: 0,
            }
        }

        fn with_exec(mut self, stdout: &str, exit_code: i32) -> Self {
            self.stdout = stdout.to_string();
            self.exit_code = exit_code;
            self
        }

        fn exec_log(&self) -> Arc<Mutex<Vec<ExecCall>>> {
            self.exec_log.clone()
        }
    }

    #[async_trait]
    impl VmHandle for MockVmHandle {
        async fn info(&self) -> Result<VmInfo, AdapterError> {
            Ok(VmInfo {
                state: "Running".into(),
                cpus: Some(1),
                memory_mb: Some(256),
            })
        }

        async fn resize(
            &self,
            _cpu: Option<u32>,
            _memory: Option<u64>,
        ) -> Result<(), AdapterError> {
            Err(AdapterError::not_supported("resize"))
        }

        async fn exec(&self, opts: &ExecOpts) -> Result<ExecResult, AdapterError> {
            self.exec_log.lock().unwrap().push(ExecCall {
                args: opts.args.clone(),
                timeout_secs: opts.timeout_secs,
                sandbox: opts.sandbox,
                work_dir: opts.work_dir.clone(),
                exec_id: opts.exec_id.clone(),
                policy: opts.policy.clone(),
            });
            Ok(ExecResult {
                stdout: self.stdout.clone(),
                stderr: String::new(),
                exit_code: self.exit_code,
            })
        }

        async fn snapshot(&self, _path: &str) -> Result<Snapshot, AdapterError> {
            Err(AdapterError::not_supported("snapshot"))
        }

        async fn shutdown(&self) -> Result<(), AdapterError> {
            Ok(())
        }

        fn pid(&self) -> u32 {
            0
        }

        fn is_alive(&self) -> bool {
            true
        }
    }

    /// Deny-by-default, version 1; reads /usr, read-writes /tmp.
    fn bound_policy() -> SandboxPolicy {
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
            audit: Default::default(),
            version: 1,
        }
    }

    /// Adds an /opt read-write grant and a higher memory limit.
    fn override_policy() -> SandboxPolicy {
        SandboxPolicy {
            capabilities: vec![Capability::File {
                path: PathPattern::Prefix("/opt".into()),
                access: FileAccess::ReadWrite,
            }],
            limits: ResourceLimits {
                memory_mb: Some(512),
                ..Default::default()
            },
            default: DefaultAccess::Deny,
            audit: Default::default(),
            version: 2,
        }
    }

    /// A bound session backed by a mock VM that records every exec.
    async fn bind_session() -> (Box<dyn SandboxHandle>, Arc<Mutex<Vec<ExecCall>>>) {
        let mock = MockVmHandle::new().with_exec("out\n", 0);
        let exec_log = mock.exec_log();
        let vm: Arc<dyn VmHandle> = Arc::new(mock);
        let spec = SandboxSpec {
            name: VmName::new("session-vm").unwrap(),
            limits: ResourceLimits::default(),
            policy: Some(bound_policy()),
        };
        let handle = GuestSandlockAdapter::new()
            .create(vm, &spec)
            .await
            .expect("create should succeed");
        (handle, exec_log)
    }

    #[tokio::test]
    async fn create_binds_policy_and_runs_guestsandlock_exec() {
        let (handle, exec_log) = bind_session().await;

        let cmd = ExecCommand {
            args: vec!["echo".into(), "hi".into()],
            ..Default::default()
        };
        let result = handle.exec(&cmd).await.unwrap();
        assert_eq!(result.stdout, "out\n");

        let log = exec_log.lock().unwrap();
        let call = log.last().expect("one exec recorded");
        assert_eq!(call.args, vec!["echo", "hi"]);
        assert!(call.sandbox, "exec must run under sandlock confinement");
        assert_eq!(call.timeout_secs, 60);
        assert_eq!(call.exec_id, None, "blocking exec carries no exec_id");
        // No override → exactly the bound policy is passed through.
        let policy = call.policy.as_ref().expect("policy always present");
        assert!(policy.grants_path(std::path::Path::new("/usr/bin/ls"), FileAccess::Read));
        assert!(policy.grants_path(std::path::Path::new("/tmp/x"), FileAccess::ReadWrite));
        assert_eq!(policy.limits.memory_mb, Some(256));
    }

    #[tokio::test]
    async fn exec_unions_policy_override_with_bound_policy() {
        let (handle, exec_log) = bind_session().await;

        let cmd = ExecCommand {
            args: vec!["echo".into()],
            policy_override: Some(override_policy()),
            ..Default::default()
        };
        handle.exec(&cmd).await.unwrap();

        let log = exec_log.lock().unwrap();
        let call = log.last().unwrap();
        let policy = call.policy.as_ref().expect("policy present");
        // Bound capabilities are preserved (base layer).
        assert!(policy.grants_path(std::path::Path::new("/usr/bin/ls"), FileAccess::Read));
        // Override capabilities are appended (union, not replace).
        assert!(policy.grants_path(std::path::Path::new("/opt/x"), FileAccess::ReadWrite));
        // Override limits win.
        assert_eq!(policy.limits.memory_mb, Some(512));
    }

    #[tokio::test]
    async fn exec_passes_work_dir_through() {
        let (handle, exec_log) = bind_session().await;

        let cmd = ExecCommand {
            args: vec!["pwd".into()],
            work_dir: Some("/workdir/sb-abcd".into()),
            ..Default::default()
        };
        handle.exec(&cmd).await.unwrap();

        let log = exec_log.lock().unwrap();
        let call = log.last().unwrap();
        assert_eq!(call.work_dir.as_deref(), Some("/workdir/sb-abcd"));
        assert!(call.sandbox);
    }

    #[tokio::test]
    async fn create_requires_an_effective_policy() {
        let vm: Arc<dyn VmHandle> = Arc::new(MockVmHandle::new());
        let spec = SandboxSpec {
            name: VmName::new("session-vm").unwrap(),
            limits: ResourceLimits::default(),
            // The engine injects its default before create; a bare spec here
            // must be rejected — the backend cannot confine without a policy.
            policy: None,
        };
        let err = match GuestSandlockAdapter::new().create(vm, &spec).await {
            Ok(_) => panic!("create without a policy must fail"),
            Err(e) => e,
        };
        assert!(matches!(err, AdapterError::InvalidArgument(_)), "{err}");
    }

    #[tokio::test]
    async fn create_rejects_invalid_policy() {
        let vm: Arc<dyn VmHandle> = Arc::new(MockVmHandle::new());
        // A relative file path fails SandboxPolicy::validate.
        let mut policy = bound_policy();
        policy.capabilities.push(Capability::File {
            path: PathPattern::Prefix("relative/path".into()),
            access: FileAccess::Read,
        });
        let spec = SandboxSpec {
            name: VmName::new("session-vm").unwrap(),
            limits: ResourceLimits::default(),
            policy: Some(policy),
        };
        let err = match GuestSandlockAdapter::new().create(vm, &spec).await {
            Ok(_) => panic!("create with an invalid policy must fail"),
            Err(e) => e,
        };
        assert!(matches!(err, AdapterError::InvalidArgument(_)), "{err}");
    }

    #[tokio::test]
    async fn destroy_is_noop() {
        let (handle, _) = bind_session().await;
        handle.destroy().await.unwrap();
    }
}

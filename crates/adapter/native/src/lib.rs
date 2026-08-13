//! Default L2 backend: Terraarium's native guest confinement
//! (`terra-sandbox` — Landlock fs + seccomp network supervision + cgroup),
//! exposed through the [`SandboxAdapter`] contract.
//!
//! Like the sandlock adapter this is a thin host-side wrapper: `create`
//! binds a VM plus its effective policy into a session; `exec` runs the
//! command via [`VmHandle::exec`] with `sandbox: true`, the bound policy,
//! and `backend: "native"` so the guest-proxy wraps the command with
//! `terra-sandbox` instead of the sandlock binary.

use std::sync::Arc;

use adapter_traits::{
    AdapterError, ExecCommand, ExecOpts, ExecResult, SandboxAdapter, SandboxHandle, SandboxPolicy,
    SandboxSpec, VmHandle,
};
use async_trait::async_trait;

/// Native guest-sandbox backend: wraps the engine's guest `terra-sandbox`
/// exec path. Stateless — each `create` produces a bound [`SandboxHandle`].
#[derive(Default)]
pub struct GuestNativeAdapter;

impl GuestNativeAdapter {
    pub fn new() -> Self {
        Self
    }
}

/// Bound session: the VM handle plus the effective policy fixed at
/// `create`. Exec runs within this context; a per-call override is merged
/// onto `policy` (base first, override capabilities appended).
struct GuestNativeHandle {
    vm: Arc<dyn VmHandle>,
    policy: SandboxPolicy,
}

#[async_trait]
impl SandboxAdapter for GuestNativeAdapter {
    async fn create(
        &self,
        vm: Arc<dyn VmHandle>,
        spec: &SandboxSpec,
    ) -> Result<Box<dyn SandboxHandle>, AdapterError> {
        let policy = spec.policy.clone().ok_or_else(|| {
            AdapterError::invalid_argument(
                "GuestNativeAdapter::create requires spec.policy (effective policy)",
            )
        })?;
        if let Err(err) = policy.validate() {
            return Err(AdapterError::invalid_argument(format!(
                "invalid sandbox policy: {}",
                err
            )));
        }
        Ok(Box::new(GuestNativeHandle { vm, policy }))
    }
}

#[async_trait]
impl SandboxHandle for GuestNativeHandle {
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, AdapterError> {
        let policy = match &cmd.policy_override {
            Some(override_policy) => self.policy.merged_with(override_policy),
            None => self.policy.clone(),
        };

        let mut opts =
            ExecOpts::new(cmd.args.clone(), cmd.timeout_secs.unwrap_or(60)).with_sandbox(true);
        opts = opts.with_backend("native");
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

    #[derive(Debug, Clone)]
    struct ExecCall {
        args: Vec<String>,
        sandbox: bool,
        backend: Option<String>,
        policy: Option<SandboxPolicy>,
    }

    struct MockVmHandle {
        exec_log: Arc<Mutex<Vec<ExecCall>>>,
    }

    impl MockVmHandle {
        fn new() -> Self {
            Self {
                exec_log: Arc::new(Mutex::new(Vec::new())),
            }
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
                sandbox: opts.sandbox,
                backend: opts.backend.clone(),
                policy: opts.policy.clone(),
            });
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
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

    #[tokio::test]
    async fn exec_routes_through_native_backend_with_policy() {
        let mock = MockVmHandle::new();
        let exec_log = mock.exec_log();
        let vm: Arc<dyn VmHandle> = Arc::new(mock);
        let policy = SandboxPolicy {
            capabilities: vec![Capability::File {
                path: PathPattern::Prefix("/opt".into()),
                access: FileAccess::Read,
            }],
            limits: ResourceLimits {
                memory_mb: Some(256),
                ..Default::default()
            },
            default: DefaultAccess::Deny,
            audit: Default::default(),
            version: 1,
        };
        let spec = SandboxSpec {
            name: VmName::new("session-vm").unwrap(),
            limits: ResourceLimits::default(),
            policy: Some(policy.clone()),
        };
        let handle = GuestNativeAdapter::new().create(vm, &spec).await.unwrap();

        let cmd = ExecCommand {
            args: vec!["echo".into(), "hi".into()],
            ..Default::default()
        };
        handle.exec(&cmd).await.unwrap();

        let log = exec_log.lock().unwrap().clone();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].args, vec!["echo".to_string(), "hi".to_string()]);
        assert!(log[0].sandbox);
        assert_eq!(log[0].backend.as_deref(), Some("native"));
        assert_eq!(log[0].policy.as_ref().unwrap().capabilities.len(), 1);
    }
}

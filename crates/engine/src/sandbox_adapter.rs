//! Default L2 backend: the guest-side sandlock confinement, exposed
//! through the [`SandboxAdapter`] contract.
//!
//! `create` binds a VM plus its effective policy into a session;
//! `exec` runs the command via `VmHandle::exec` with `sandbox: true` and
//! the bound policy (a per-call `policy_override` is unioned on top).
//! The guest-proxy sandlock path is the transport — this module is a pure
//! wrapper around it (zero behavior change; C-phase contract landing).

use std::sync::Arc;

use adapter_traits::{
    AdapterError, ExecCommand, ExecOpts, ExecResult, SandboxAdapter, SandboxHandle, SandboxPolicy,
    SandboxSpec, VmHandle,
};
use async_trait::async_trait;

/// Default sandbox backend: wraps the engine's existing guest-sandlock
/// exec path. Stateless — each `create` produces a bound [`SandboxHandle`].
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
        // Effective policy = bound policy ∪ per-call override (engine's
        // base-union-user merge: bound capabilities preserved, override
        // capabilities appended, override limits win).
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

    async fn setup(&self, _tools: &[String]) -> Result<(), AdapterError> {
        // Guest sandlock confines per-exec; it needs no persistent setup.
        Ok(())
    }

    async fn destroy(&self) -> Result<(), AdapterError> {
        // Nothing to release: the guest-proxy applies rules per-run.
        Ok(())
    }
}

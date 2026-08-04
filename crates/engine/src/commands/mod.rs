//! Command execution — shared by daemon and CLI modes.
//!
//! Each function takes a `&mut VmManager` and a typed command payload,
//! executes it, and returns a serializable response.

mod audit_cmd;
mod exec;
mod fs;
mod network;
mod pool;
mod sandbox;
mod session;
mod snapshot;
mod vm;

use std::sync::Arc;

use crate::audit;
use crate::manager::VmManager;
use crate::policy::default_sandbox_policy;
use adapter_traits::{
    AdapterError, ExecCommand, ExecOpts, ExecResult, SandboxHandle, SandboxPolicy, Snapshot,
    VmHandle, VmName, VmSpec,
};
pub(crate) use terrarium_protocol::{Command, Response};

/// Extract the required `name` field from a command.
pub(crate) fn require_name(cmd: &Command) -> Result<String, Response> {
    cmd.name
        .clone()
        .ok_or_else(|| Response::err("Missing 'name' field"))
}

/// Extract VM name from a command and look up the VM handle.
/// Returns an error if the name is missing or the VM is not found.
pub(crate) fn get_vm<'a>(
    mgr: &'a VmManager,
    cmd: &Command,
) -> Result<(&'a dyn VmHandle, String), Response> {
    let name = require_name(cmd)?;
    let vm = mgr
        .get(&name)
        .ok_or_else(|| Response::err(format!("VM '{}' not found", name)))?;
    Ok((vm, name))
}

/// Default system layer appended when the caller's layer list has none.
pub(crate) const DEFAULT_SYSTEM: &str = "base";

/// System base layers: if the caller's layer list doesn't end with one,
/// the configured `system` (default "base") is auto-appended.
// NOTE: cross-referenced with fs's SYSTEM_LAYER_NAMES (crates/fs/src/layer.rs);
// intentionally not unified — engine cannot depend on fs (layering).
pub(crate) const SYSTEM_BASES: [&str; 2] = ["base", "ubuntu"];

/// Shared blocking/background exec dispatch for `exec` and `sandbox_exec`:
/// clamps the timeout, defaults the sandbox flag, gates and validates the
/// policy, then routes to the manager. `sandbox_flag` is the caller's
/// explicit value (None → `sandbox_default`); `sandbox_id`, when present,
/// links a background session to an engine sandbox and adds the `sandbox`
/// field to the response. `policy` is the per-call policy only (C3: the
/// stored policy lives in the create-bound session handle).
///
/// Policy validation uses `SandboxPolicy::validate()`. Default injection
/// (D2) happens here: whenever the exec is sandboxed and neither the
/// per-call nor the stored policy resolves, the engine default
/// `default_sandbox_policy()` is injected — so every sandboxed exec
/// carries a complete policy, regardless of command name. An unsandboxed
/// exec keeps `policy` optional.
///
/// C3 routing: a *sandboxed blocking* `sandbox_exec` resolves the record's
/// bound `SandboxHandle` and calls `handle.exec` — the backend unions the
/// per-call override onto the policy bound at create. Background sessions
/// keep the direct `vm.exec` path (they register an exec_id for
/// `session_kill`, which `SandboxHandle::exec` has no concept of), and the
/// unsandboxed escape hatch (`sandbox:false`) also stays direct.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_exec(
    mgr: &mut VmManager,
    vm_name: &str,
    args: &[String],
    timeout_secs: Option<u64>,
    sandbox_default: bool,
    sandbox_flag: Option<bool>,
    policy: Option<SandboxPolicy>,
    work_dir: Option<&str>,
    exec_mode: Option<String>,
    sandbox_id: Option<String>,
) -> Response {
    if args.is_empty() {
        return Response::err("Missing 'args' field");
    }
    let timeout = timeout_secs.unwrap_or(60).min(3600);
    let sandbox = sandbox_flag.unwrap_or(sandbox_default);
    // Per-call override only (the handle path); the stored policy stays
    // bound in the session handle created at sandbox_create.
    let per_call = policy.clone();
    // Stored policy for sandbox_exec — only the direct/background paths
    // need it (they keep the pre-C3 `base ∪ user` construction).
    let stored = sandbox_id
        .as_deref()
        .and_then(|id| mgr.sandbox_get(id))
        .and_then(|r| r.policy);
    // Capability model: the engine default policy is the BASE layer
    // (read-only system dirs, RW /tmp) that every sandboxed exec starts
    // from; a user policy APPENDS its capabilities on top (union), so a
    // user granting only /opt still runs /bin/sh (the default's -r /bin
    // carries sandlock's execute grant). An unsandboxed exec stays
    // policy-free; when sandboxed, the effective policy is base ∪ user.
    // Shared resolution with the daemon's lock-free path
    // (`prepare_blocking_exec`) — the two must never drift.
    let policy = match resolve_effective_policy(sandbox, per_call.as_ref(), stored.as_ref()) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let mode = exec_mode.as_deref().unwrap_or("blocking");
    match mode {
        "background" => {
            // M2: the background exec runs with the computed `policy` —
            // enforce the VM quota before dispatch, so an over-quota
            // override never spawns a session or a guest exec.
            if let Some(policy) = policy.as_ref() {
                if let Err(err) = validate_exec_quota(mgr, vm_name, policy) {
                    return err;
                }
            }
            let session_id = uuid::Uuid::new_v4().to_string();
            match mgr
                .exec_background(
                    vm_name,
                    args,
                    timeout,
                    sandbox,
                    &session_id,
                    work_dir,
                    sandbox_id.clone(),
                    policy,
                )
                .await
            {
                Ok(()) => {
                    let mut data = serde_json::json!({
                        "session_id": session_id,
                        "status": "started",
                    });
                    if let Some(id) = sandbox_id {
                        data["sandbox"] = serde_json::Value::String(id);
                    }
                    Response::ok(data)
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        "blocking" => {
            // C3: a sandboxed blocking sandbox_exec routes through the
            // session's bound handle. Missing handle (pre-C3 record) falls
            // back to the direct path with the full effective policy.
            if let Some(sb_id) = sandbox_id.as_deref() {
                if sandbox {
                    if let Some(handle) = mgr.sandbox_handle(sb_id) {
                        // M2/M3: the backend executes `bound ∪ per_call`,
                        // NOT the replace-chain `policy` — compute the
                        // ACTUAL executed policy (it carries the stored
                        // policy's limits-as-fallback and audit flags),
                        // validate its limits against the VM quota, and
                        // gate the audit events on it.
                        let executed = handle_executed_policy(stored.as_ref(), per_call.as_ref());
                        if let Err(err) = validate_exec_quota(mgr, vm_name, &executed) {
                            return err;
                        }
                        let cmd = sandbox_exec_command(args, work_dir, per_call, timeout);
                        let prepared = PreparedExec::Sandbox { handle, cmd };
                        return prepared_exec_audited(prepared, Some(&executed), sb_id, args).await;
                    }
                }
            }
            // Direct blocking path: the computed `policy` is what runs.
            if let Some(policy) = policy.as_ref() {
                if let Err(err) = validate_exec_quota(mgr, vm_name, policy) {
                    return err;
                }
            }
            blocking_exec_audited(
                mgr.exec(vm_name, args, timeout, sandbox, work_dir, policy.clone()),
                policy.as_ref(),
                sandbox_id.as_deref().unwrap_or(vm_name),
                args,
            )
            .await
        }
        other => Response::err(format!(
            "invalid exec_mode {:?}: expected \"blocking\" or \"background\"",
            other
        )),
    }
}

/// C3: build the `ExecCommand` for a sandboxed blocking `sandbox_exec`.
/// The engine passes only the per-call override; the backend unions it
/// onto the policy bound at create (never a replace).
fn sandbox_exec_command(
    args: &[String],
    work_dir: Option<&str>,
    policy_override: Option<SandboxPolicy>,
    timeout_secs: u64,
) -> ExecCommand {
    ExecCommand {
        args: args.to_vec(),
        work_dir: work_dir.map(String::from),
        env: None,
        policy_override,
        timeout_secs: Some(timeout_secs),
    }
}

/// The policy a sandboxed blocking exec actually runs with on the C3
/// handle path (the sandlock backend's `bound ∪ per-call` merge): the
/// policy fixed at `sandbox_create` (`default ∪ stored` — reconstructed
/// here from the record's raw user policy, which is what create bound)
/// unioned with the per-call override.
///
/// This is NOT `default.merged_with(per_call.or(stored))`: the `.or()`
/// replace-chain silently drops the stored policy whenever an override is
/// present, losing its limits-as-fallback and its audit flags — exactly
/// what the executed policy keeps. Audit gating and quota validation must
/// see this executed policy, or they describe a policy that never ran.
fn handle_executed_policy(
    stored: Option<&SandboxPolicy>,
    per_call: Option<&SandboxPolicy>,
) -> SandboxPolicy {
    let bound = match stored {
        Some(user) => default_sandbox_policy().merged_with(user),
        None => default_sandbox_policy(),
    };
    match per_call {
        Some(override_policy) => bound.merged_with(override_policy),
        None => bound,
    }
}

/// Resolve the effective policy for an exec command and validate it.
///
/// Unsandboxed execs keep the per-call policy (or the record's stored
/// policy) untouched; sandboxed execs run the engine default `base ∪
/// user` (D2 — every sandboxed exec carries a complete policy). A policy
/// on an unsandboxed exec is rejected, and the resolved policy must pass
/// [`SandboxPolicy::validate`].
///
/// Shared by `run_exec` (the shared `execute` path — CLI and embedded
/// modes, and the daemon's background fallback) and `prepare_blocking_exec`
/// (the daemon's lock-free blocking path), so validation order and error
/// messages stay byte-identical whichever path served the command.
fn resolve_effective_policy(
    sandbox: bool,
    per_call: Option<&SandboxPolicy>,
    stored: Option<&SandboxPolicy>,
) -> Result<Option<SandboxPolicy>, Response> {
    let policy = if sandbox {
        Some(match per_call.or(stored) {
            Some(user) => default_sandbox_policy().merged_with(user),
            None => default_sandbox_policy(),
        })
    } else {
        per_call.cloned().or_else(|| stored.cloned())
    };
    if !sandbox && policy.is_some() {
        return Err(Response::err(
            "'policy' requires sandboxed exec (set 'sandbox': true)",
        ));
    }
    if let Some(policy) = policy.as_ref() {
        if let Err(err) = policy.validate() {
            return Err(Response::err(err));
        }
    }
    Ok(policy)
}

/// M2 (policy-model.md §3.5): reject an exec whose effective policy's
/// limits exceed the tenant VM's current physical quota.
/// `SandboxPolicy::validate_with_vm` is only wired at `sandbox_create`;
/// without this check a per-call override (or a post-create VM shrink)
/// could run limits the VM cannot honor. The executed policy is
/// path-specific: the direct/background paths run the computed `policy`,
/// the C3 handle path runs `bound ∪ per_call` (see
/// [`handle_executed_policy`]). No `VmPolicy` → nothing to validate
/// against (mirrors the create-path guard).
fn validate_exec_quota(
    mgr: &VmManager,
    vm_name: &str,
    executed: &SandboxPolicy,
) -> Result<(), Response> {
    if let Some(vm_policy) = mgr.vm_policy(vm_name) {
        if let Err(err) = executed.validate_with_vm(vm_policy) {
            return Err(Response::err(err));
        }
    }
    Ok(())
}

/// A blocking exec resolved while the manager lock is held. The `Arc`
/// handles keep the VM/session alive and the fully-built options need no
/// further registry access, so the exec itself can run lock-free — a
/// long-running exec (up to its timeout) must not serialize every other
/// command behind `Mutex<VmManager>`.
pub(crate) enum PreparedExec {
    /// Direct `VmHandle::exec` (VM-scoped `exec`, unsandboxed
    /// `sandbox_exec`, and pre-C3 records without a bound handle).
    Direct {
        handle: Arc<dyn VmHandle>,
        opts: ExecOpts,
    },
    /// C3: a sandboxed blocking `sandbox_exec` resolved to the session's
    /// bound handle; executed via `SandboxHandle::exec`.
    Sandbox {
        handle: Arc<dyn SandboxHandle>,
        cmd: ExecCommand,
    },
}

/// A resolved blocking exec plus the audit context captured at resolution:
/// the effective policy the exec ran with (gating `audit.{exec,deny}`) and
/// the audit subject id (the engine sandbox id for `sandbox_exec`, else
/// the vm name).
pub(crate) struct PreparedBlockingExec {
    pub prepared: PreparedExec,
    pub policy: Option<SandboxPolicy>,
    pub audit_id: String,
}

/// A VM lifecycle command resolved under the manager lock for lock-free
/// execution (P1-2 scale): the spec and the duplicate-name check happen
/// here; the adapter call (compose + CH spawn, ~200ms) runs without the
/// lock; the returned handle is registered afterwards.
pub(crate) enum PreparedLifecycle {
    Create(VmSpec),
    Restore { snapshot: Snapshot, spec: VmSpec },
}

/// Resolve a `create` / `restore` command into a [`PreparedLifecycle`],
/// replicating `cmd_create` / `cmd_restore`'s validation and error
/// messages so the lock-free daemon path behaves identically to the
/// shared `execute` path.
pub(crate) fn prepare_lifecycle(
    mgr: &VmManager,
    cmd: &Command,
) -> Result<PreparedLifecycle, Response> {
    let (spec, snapshot) = match cmd.command.as_str() {
        "create" => {
            let spec = build_spec(cmd).map_err(Response::err)?;
            if let Err(e) = spec.validate() {
                return Err(Response::err(e));
            }
            (spec, None)
        }
        "restore" => {
            let path = match cmd.snapshot_path.clone() {
                Some(p) if !p.is_empty() => p,
                _ => return Err(Response::err("Missing 'snapshot_path' field")),
            };
            let spec = build_restore_spec(cmd).map_err(Response::err)?;
            if let Err(e) = spec.validate() {
                return Err(Response::err(e));
            }
            (spec, Some(Snapshot { path }))
        }
        other => {
            return Err(Response::err(format!("Unknown command: {}", other)));
        }
    };
    if mgr.get(&spec.name.to_string()).is_some() {
        return Err(Response::err(format!("VM '{}' already exists", spec.name)));
    }
    Ok(match snapshot {
        Some(snapshot) => PreparedLifecycle::Restore { snapshot, spec },
        None => PreparedLifecycle::Create(spec),
    })
}

/// Resolve a blocking `exec` / `sandbox_exec` command to its handle and
/// options. Runs under the manager lock (cheap registry lookups); the
/// caller drops the lock before awaiting the exec.
///
/// Replicates the exact validation order of `run_exec` — and, for
/// sandbox_exec, the record lookup of `cmd_sandbox_exec` — so error
/// messages and their precedence stay byte-identical to the shared
/// `execute` path. Background mode never reaches here: the daemon falls
/// back to `execute` for it (it registers its session under the lock and
/// returns immediately; its spawned task already runs lock-free).
pub(crate) fn prepare_blocking_exec(
    mgr: &VmManager,
    cmd: &Command,
) -> Result<PreparedBlockingExec, Response> {
    match cmd.command.as_str() {
        "exec" | "sandbox_exec" => {}
        other => return Err(Response::err(format!("Unknown command: {}", other))),
    }
    let sandbox_record = if cmd.command.as_str() == "sandbox_exec" {
        let id = cmd
            .id
            .clone()
            .ok_or_else(|| Response::err("Missing 'id' field"))?;
        let record = mgr
            .sandbox_get(&id)
            .ok_or_else(|| Response::err(format!("Sandbox '{}' not found", id)))?;
        Some(record)
    } else {
        None
    };

    let (name, sandbox_default, work_dir, stored_policy) = match &sandbox_record {
        Some(record) => (
            record.vm_name.clone(),
            true,
            Some(record.workdir.clone()),
            record.policy.clone(),
        ),
        None => (require_name(cmd)?, false, None, None),
    };

    if cmd.args.is_empty() {
        return Err(Response::err("Missing 'args' field"));
    }
    let timeout = cmd.timeout_secs.unwrap_or(60).min(3600);
    let sandbox = cmd.sandbox.unwrap_or(sandbox_default);
    let per_call = cmd.policy.clone();
    // Capability model: base ∪ user for sandboxed exec; the bound handle
    // path uses the per-call override + create-bound policy instead —
    // this effective policy serves the direct paths only. Shared with
    // `run_exec` (see `resolve_effective_policy`).
    let policy = resolve_effective_policy(sandbox, per_call.as_ref(), stored_policy.as_ref())?;

    // Audit subject id: the engine sandbox id for `sandbox_exec`, else the
    // vm name (the entity the caller addressed).
    let audit_id = if cmd.command.as_str() == "sandbox_exec" {
        cmd.id.clone().unwrap_or_else(|| name.clone())
    } else {
        name.clone()
    };

    // C3: a sandboxed blocking sandbox_exec resolves to the session's
    // bound handle — the Arc is cloned under the lock and awaited outside
    // (the point of `prepare`). Records without a handle (pre-C3) and the
    // unsandboxed escape hatch keep the direct vm.exec path.
    if sandbox {
        if let Some(record) = &sandbox_record {
            if let Some(handle) = &record.handle {
                // M2/M3: the backend executes `bound ∪ per_call`, NOT the
                // replace-chain `policy` — return the ACTUAL executed
                // policy (it carries the stored policy's limits-as-fallback
                // and audit flags) so the caller gates audit and enforces
                // the VM quota on the policy that really runs.
                let executed = handle_executed_policy(stored_policy.as_ref(), per_call.as_ref());
                validate_exec_quota(mgr, &name, &executed)?;
                return Ok(PreparedBlockingExec {
                    prepared: PreparedExec::Sandbox {
                        handle: handle.clone(),
                        cmd: sandbox_exec_command(
                            &cmd.args,
                            work_dir.as_deref(),
                            per_call,
                            timeout,
                        ),
                    },
                    policy: Some(executed),
                    audit_id,
                });
            }
        }
    }

    // Direct path (unsandboxed, or pre-C3 record without a handle): the
    // computed `policy` is what runs — enforce the VM quota on it.
    if let Some(policy) = policy.as_ref() {
        validate_exec_quota(mgr, &name, policy)?;
    }

    let handle = mgr.get_handle(&name).ok_or_else(|| {
        Response::err(AdapterError::not_found(format!("VM '{}' not found", name)).to_string())
    })?;

    let mut opts = ExecOpts::new(cmd.args.clone(), timeout).with_sandbox(sandbox);
    if let Some(work_dir) = work_dir {
        opts = opts.with_work_dir(work_dir);
    }
    opts.policy = policy.clone();
    Ok(PreparedBlockingExec {
        prepared: PreparedExec::Direct { handle, opts },
        policy,
        audit_id,
    })
}

/// Build the blocking-exec response — `{stdout, stderr, exit_code}` on
/// success, `{status:"error", error:<msg>}` on failure. Shared by the
/// lock-free daemon path and `run_exec` so the wire format is identical
/// whichever path served the command.
pub(crate) fn blocking_exec_response(result: Result<ExecResult, AdapterError>) -> Response {
    match result {
        Ok(r) => Response::ok(serde_json::json!({
            "stdout": r.stdout,
            "stderr": r.stderr,
            "exit_code": r.exit_code,
        })),
        Err(e) => Response::err(e.to_string()),
    }
}

/// Await a blocking exec with audit instrumentation (shared by `run_exec`'s
/// direct path and the daemon's lock-free prepared path). The `Instant` is
/// taken here — at the exec call, not at command entry — so `duration_ms`
/// is the exec's own wall-clock time. `audit_id` is the engine sandbox id
/// for `sandbox_exec`, else the vm name; gating uses the effective policy.
async fn blocking_exec_audited<F>(
    exec: F,
    policy: Option<&SandboxPolicy>,
    audit_id: &str,
    args: &[String],
) -> Response
where
    F: std::future::Future<Output = Result<ExecResult, AdapterError>>,
{
    let start = std::time::Instant::now();
    let result = exec.await;
    audit::audit_exec_outcome(
        policy,
        audit_id,
        args,
        &result,
        start.elapsed().as_millis() as u64,
    );
    blocking_exec_response(result)
}

/// Await a [`PreparedExec`] with audit instrumentation — the daemon's
/// lock-free blocking path. Same audit semantics as [`blocking_exec_audited`].
pub(crate) async fn prepared_exec_audited(
    prepared: PreparedExec,
    policy: Option<&SandboxPolicy>,
    audit_id: &str,
    args: &[String],
) -> Response {
    let start = std::time::Instant::now();
    let result = match prepared {
        PreparedExec::Direct { handle, opts } => handle.exec(&opts).await,
        PreparedExec::Sandbox { handle, cmd } => handle.exec(&cmd).await,
    };
    audit::audit_exec_outcome(
        policy,
        audit_id,
        args,
        &result,
        start.elapsed().as_millis() as u64,
    );
    blocking_exec_response(result)
}

/// The system base is implicit: tool layers stack on top of it. Append it
/// unless the caller already ended the layer list with one.
pub(crate) fn apply_system_base(cmd: &mut Command) {
    if !cmd.layers.is_empty() {
        let last = cmd.layers.last().map(|s| s.as_str()).unwrap_or("");
        if !SYSTEM_BASES.contains(&last) {
            let system = cmd.system.clone().unwrap_or_else(|| DEFAULT_SYSTEM.into());
            cmd.layers.push(system);
        }
    }
}

/// Execute a command against the given VM manager.
pub async fn execute(mgr: &mut VmManager, cmd: Command) -> Response {
    match cmd.command.as_str() {
        // VM commands: compute lifecycle only — never touch disks.
        "create" => vm::cmd_create(mgr, cmd).await,
        "list" => vm::cmd_list(mgr).await,
        "info" => vm::cmd_info(mgr, cmd).await,
        "resize" => vm::cmd_resize(mgr, cmd).await,
        "shutdown" => vm::cmd_shutdown(mgr, cmd).await,
        "kill" => vm::cmd_kill(mgr, cmd).await,
        "destroy" => vm::cmd_destroy(mgr, cmd).await,
        "snapshot" => snapshot::cmd_snapshot(mgr, cmd).await,
        "restore" => snapshot::cmd_restore(mgr, cmd).await,
        "reset_vm" => snapshot::cmd_reset_vm(mgr, cmd).await,
        "audit_list" => audit_cmd::cmd_audit_list(&cmd),
        "attach_fs" => fs::cmd_attach_fs(mgr, cmd).await,
        "detach_fs" => fs::cmd_detach_fs(mgr, cmd).await,
        "exec" => exec::cmd_exec(mgr, cmd).await,
        "net_list" => network::cmd_net_list(mgr),
        "net_down" => network::cmd_net_down(mgr),
        "net_up" => network::cmd_net_up(),
        "pool_create" => pool::cmd_pool_create(mgr, cmd).await,
        "pool_list" => pool::cmd_pool_list(mgr),
        "pool_claim" => pool::cmd_pool_claim(mgr, cmd).await,
        "pool_release" => pool::cmd_pool_release(mgr, cmd).await,
        "pool_shrink" => pool::cmd_pool_shrink(mgr, cmd).await,
        "session_status" => session::cmd_session_status(mgr, cmd),
        "session_kill" => session::cmd_session_kill(mgr, cmd).await,
        "session_list" => session::cmd_session_list(mgr),
        "sandbox_create" => sandbox::cmd_sandbox_create(mgr, cmd).await,
        "sandbox_exec" => sandbox::cmd_sandbox_exec(mgr, cmd).await,
        "sandbox_list" => sandbox::cmd_sandbox_list(mgr, cmd),
        "sandbox_info" => sandbox::cmd_sandbox_info(mgr, cmd),
        "sandbox_kill" => sandbox::cmd_sandbox_kill(mgr, cmd).await,
        "tenant_destroy" => sandbox::cmd_tenant_destroy(mgr, cmd).await,
        // Handled by the daemon listener itself (it owns the shutdown
        // channel); reaching this arm means there is no daemon to stop.
        "daemon_stop" => Response::err("daemon_stop is only handled by the daemon listener"),
        _ => Response::err(format!("Unknown command: {}", cmd.command)),
    }
}

pub(crate) fn build_spec(cmd: &Command) -> Result<VmSpec, String> {
    let kernel = cmd.kernel.as_ref().ok_or("Missing 'kernel' field")?;
    build_spec_with_kernel(cmd, Some(kernel.clone()))
}

/// Build a VM spec from a command without requiring a kernel — the
/// restore command's guest state comes from a snapshot.
pub(crate) fn build_restore_spec(cmd: &Command) -> Result<VmSpec, String> {
    // The kernel is not part of the restored guest state, but CH's CLI
    // still requires a --kernel flag on restore — carry the caller's
    // kernel path if provided (the SDK passes the default kernel).
    build_spec_with_kernel(cmd, cmd.kernel.clone())
}

fn build_spec_with_kernel(cmd: &Command, kernel: Option<String>) -> Result<VmSpec, String> {
    let name = cmd.name.as_ref().ok_or("Missing 'name' field")?;

    let vm_name = VmName::new(name.clone())?;
    let boot_vcpus = cmd.cpus.unwrap_or(2);
    let max_vcpus = cmd.max_cpus;
    let memory_mb = cmd.memory_mb.unwrap_or(512);
    let max_memory_mb = cmd.max_memory_mb;

    Ok(VmSpec {
        name: vm_name,
        kernel: kernel.clone(),
        cmdline: cmd.cmdline.clone(),
        boot_vcpus,
        max_vcpus,
        memory_mb,
        max_memory_mb,
        initramfs: cmd.initramfs.clone(),
        net: cmd.net,
        fs: if cmd.layers.is_empty() {
            None
        } else {
            Some(adapter_traits::FsSpec {
                layers: cmd.layers.clone(),
                upper: match cmd.upper.as_deref() {
                    Some(u) => adapter_traits::UpperPolicy::Persistent(u.to_string()),
                    None => adapter_traits::UpperPolicy::Ephemeral,
                },
            })
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use adapter_traits::{Capability, DefaultAccess, FileAccess, PathPattern, ResourceLimits};

    fn user_policy() -> SandboxPolicy {
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
            ..Default::default()
        }
    }

    #[test]
    fn unsandboxed_with_policy_is_rejected() {
        let err = resolve_effective_policy(false, Some(&user_policy()), None).unwrap_err();
        assert!(
            err.error
                .as_deref()
                .unwrap_or("")
                .contains("'policy' requires sandboxed exec"),
            "unexpected error: {:?}",
            err.error
        );
    }

    #[test]
    fn unsandboxed_without_policy_is_none() {
        let policy = resolve_effective_policy(false, None, None).unwrap();
        assert!(policy.is_none(), "unsandboxed exec carries no policy");
    }

    #[test]
    fn sandboxed_without_user_policy_is_engine_default() {
        let policy = resolve_effective_policy(true, None, None).unwrap();
        assert_eq!(policy, Some(default_sandbox_policy()));
    }

    #[test]
    fn sandboxed_unions_user_capabilities_on_default() {
        let policy = resolve_effective_policy(true, Some(&user_policy()), None)
            .unwrap()
            .unwrap();
        // Base set retained: read-only system dirs still granted.
        assert!(policy.grants_path(std::path::Path::new("/usr/bin/ls"), FileAccess::Read));
        assert!(policy.grants_path(std::path::Path::new("/etc/passwd"), FileAccess::Read));
        // User grant appended, user limits win.
        assert!(policy.grants_path(std::path::Path::new("/opt/app"), FileAccess::ReadWrite));
        assert_eq!(policy.limits.memory_mb, Some(512));
        assert_eq!(policy.default, DefaultAccess::Deny);
    }

    #[test]
    fn stored_policy_is_fallback_for_sandboxed_exec() {
        let policy = resolve_effective_policy(true, None, Some(&user_policy()))
            .unwrap()
            .unwrap();
        assert!(policy.grants_path(std::path::Path::new("/opt/app"), FileAccess::ReadWrite));
        assert_eq!(policy.limits.memory_mb, Some(512));
    }

    #[test]
    fn invalid_user_policy_is_rejected() {
        let mut bad = user_policy();
        bad.capabilities.push(Capability::File {
            path: PathPattern::Prefix("opt/relative".into()),
            access: FileAccess::Read,
        });
        let err = resolve_effective_policy(true, Some(&bad), None).unwrap_err();
        assert!(
            err.error
                .as_deref()
                .unwrap_or("")
                .contains("must be absolute"),
            "{}",
            err.error.as_deref().unwrap_or("")
        );
    }
}

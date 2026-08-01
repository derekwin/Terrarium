//! Command execution — shared by daemon and CLI modes.
//!
//! Each function takes a `&mut VmManager` and a typed command payload,
//! executes it, and returns a serializable response.

mod exec;
mod fs;
mod network;
mod pool;
mod sandbox;
mod session;
mod snapshot;
mod vm;

use std::sync::Arc;

use crate::manager::VmManager;
use adapter_traits::{AdapterError, ExecOpts, ExecResult, VmHandle, VmName, VmSpec};
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

/// Validate a user-supplied exec policy before it reaches the guest.
/// `net_allow`, when present, must be a non-empty list: an empty list
/// emits zero `--net-allow` flags, which would silently leave the network
/// unrestricted — the opposite of what a user passing `[]` intends.
pub(crate) fn validate_policy(policy: &adapter_traits::ExecPolicy) -> Result<(), Response> {
    if let Some(entries) = &policy.net_allow {
        if entries.is_empty() {
            return Err(Response::err(
                "net_allow must be a non-empty list (omit the field for unrestricted network)",
            ));
        }
    }
    Ok(())
}

/// Shared blocking/background exec dispatch for `exec` and `sandbox_exec`:
/// clamps the timeout, defaults the sandbox flag, gates and validates the
/// policy, then routes to the manager. `sandbox_flag` is the caller's
/// explicit value (None → `sandbox_default`); `sandbox_id`, when present,
/// links a background session to an engine sandbox and adds the `sandbox`
/// field to the response.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_exec(
    mgr: &mut VmManager,
    vm_name: &str,
    args: &[String],
    timeout_secs: Option<u64>,
    sandbox_default: bool,
    sandbox_flag: Option<bool>,
    policy: Option<adapter_traits::ExecPolicy>,
    work_dir: Option<&str>,
    exec_mode: Option<String>,
    sandbox_id: Option<String>,
) -> Response {
    if args.is_empty() {
        return Response::err("Missing 'args' field");
    }
    let timeout = timeout_secs.unwrap_or(60).min(3600);
    let sandbox = sandbox_flag.unwrap_or(sandbox_default);
    if !sandbox && policy.is_some() {
        return Response::err("'policy' requires sandboxed exec (set 'sandbox': true)");
    }
    if let Some(policy) = policy.as_ref() {
        if let Err(resp) = validate_policy(policy) {
            return resp;
        }
    }

    let mode = exec_mode.as_deref().unwrap_or("blocking");
    match mode {
        "background" => {
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
        "blocking" => blocking_exec_response(
            mgr.exec(vm_name, args, timeout, sandbox, work_dir, policy)
                .await,
        ),
        other => Response::err(format!(
            "invalid exec_mode {:?}: expected \"blocking\" or \"background\"",
            other
        )),
    }
}

/// A blocking exec resolved while the manager lock is held. The `Arc`
/// handle keeps the VM alive and the fully-built `ExecOpts` needs no
/// further registry access, so the exec itself can run lock-free — a
/// long-running exec (up to its timeout) must not serialize every other
/// command behind `Mutex<VmManager>`.
pub(crate) struct PreparedExec {
    pub handle: Arc<dyn VmHandle>,
    pub opts: ExecOpts,
}

/// Resolve a blocking `exec` / `sandbox_exec` command to its handle and
/// options. Runs under the manager lock (cheap registry lookups); the
/// caller drops the lock before awaiting `handle.exec`.
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
) -> Result<PreparedExec, Response> {
    let (name, sandbox_default, work_dir, stored_policy) = match cmd.command.as_str() {
        "exec" => (require_name(cmd)?, false, None, None),
        "sandbox_exec" => {
            let id = cmd
                .id
                .clone()
                .ok_or_else(|| Response::err("Missing 'id' field"))?;
            let record = mgr
                .sandbox_get(&id)
                .ok_or_else(|| Response::err(format!("Sandbox '{}' not found", id)))?;
            (record.vm_name, true, Some(record.workdir), record.policy)
        }
        other => return Err(Response::err(format!("Unknown command: {}", other))),
    };

    if cmd.args.is_empty() {
        return Err(Response::err("Missing 'args' field"));
    }
    let timeout = cmd.timeout_secs.unwrap_or(60).min(3600);
    let sandbox = cmd.sandbox.unwrap_or(sandbox_default);
    let policy = cmd.policy.clone().or(stored_policy);
    if !sandbox && policy.is_some() {
        return Err(Response::err(
            "'policy' requires sandboxed exec (set 'sandbox': true)",
        ));
    }
    if let Some(policy) = policy.as_ref() {
        validate_policy(policy)?;
    }

    let handle = mgr.get_handle(&name).ok_or_else(|| {
        Response::err(AdapterError::not_found(format!("VM '{}' not found", name)).to_string())
    })?;

    let mut opts = ExecOpts::new(cmd.args.clone(), timeout).with_sandbox(sandbox);
    if let Some(work_dir) = work_dir {
        opts = opts.with_work_dir(work_dir);
    }
    opts.policy = policy;
    Ok(PreparedExec { handle, opts })
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
        "restore" => snapshot::cmd_restore(mgr, cmd),
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
    let name = cmd.name.as_ref().ok_or("Missing 'name' field")?;
    let kernel = cmd.kernel.as_ref().ok_or("Missing 'kernel' field")?;

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

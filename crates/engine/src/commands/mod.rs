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

use crate::manager::VmManager;
use adapter_traits::{VmHandle, VmName, VmSpec};
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

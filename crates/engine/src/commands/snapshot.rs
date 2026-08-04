//! VM snapshot / restore commands — the P1 fast-reset primitive.
//!
//! A snapshot captures a VM at a known-good state; `restore` creates a
//! NEW VM whose guest state comes from that snapshot (the host-side stack
//! — layers, vsock, CH process — is rebuilt fresh). This is the
//! environment-reset primitive for RL/episode recycling, not crash fault
//! tolerance (agents are restarted, not recovered).

use crate::manager::VmManager;
use adapter_traits::Snapshot;
use terrarium_protocol::{Command, Response};

use super::build_restore_spec;

/// {"command":"snapshot","name":<vm>,"snapshot_path"?:<dest .bin path>}
///
/// Default destination: `{snapshot_dir}/terra-snap-<vm>` — a DIRECTORY
/// that CH fills with the memory + state files (restore points at the
/// same directory).
pub(crate) async fn cmd_snapshot(mgr: &VmManager, cmd: Command) -> Response {
    let (vm, name) = match super::get_vm(mgr, &cmd) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let path = match cmd.snapshot_path {
        Some(p) => p,
        None => format!("{}/terra-snap-{}", mgr.snapshot_dir(), name),
    };
    match vm.snapshot(&path).await {
        Ok(snap) => Response::ok(serde_json::json!({"snapshot_path": snap.path})),
        Err(e) => Response::err(e.to_string()),
    }
}

/// {"command":"restore","name":<new vm>,"snapshot_path":<.bin path>,
///  "cpus"?:..., "memory_mb"?:..., "layers"?:..., "upper"?:..., "net"?:...}
///
/// Creates a NEW VM restored from the snapshot. The host-side resources
/// (cpus/memory) must match what the snapshotted VM was configured with —
/// they are part of the restored guest state.
pub(crate) async fn cmd_restore(mgr: &mut VmManager, cmd: Command) -> Response {
    let snapshot_path = match cmd.snapshot_path.clone() {
        Some(p) if !p.is_empty() => p,
        _ => return Response::err("Missing 'snapshot_path' field"),
    };
    let spec = match build_restore_spec(&cmd) {
        Ok(s) => s,
        Err(e) => return Response::err(e),
    };
    if let Err(e) = spec.validate() {
        return Response::err(e);
    }
    let name = spec.name.to_string();
    let snapshot = Snapshot {
        path: snapshot_path,
    };
    match mgr.restore(&snapshot, spec).await {
        Ok(()) => {
            let pid = mgr.get(&name).map(|h| h.pid());
            Response::ok(serde_json::json!({"name": name, "status": "restored", "pid": pid}))
        }
        Err(e) => Response::err(e.to_string()),
    }
}

/// {"command":"reset_vm","name":<vm>}
///
/// In-place episode reset (P1/RL fast path): the VM keeps running — the
/// guest kills its episode processes and clears the episode-writable
/// runtime dirs back to the layer baseline.
pub(crate) async fn cmd_reset_vm(mgr: &VmManager, cmd: Command) -> Response {
    let (vm, name) = match super::get_vm(mgr, &cmd) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match vm.reset_fs().await {
        Ok(()) => Response::ok(serde_json::json!({"name": name, "status": "reset"})),
        Err(e) => Response::err(e.to_string()),
    }
}

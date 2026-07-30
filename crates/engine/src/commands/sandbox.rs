//! Engine-level sandbox commands (S-M2).
//!
//! A sandbox is a workdir on a tenant's shared VM (`tenant-<tenant>`);
//! isolation inside the VM is enforced guest-side by sandlock. The engine
//! owns the registry: tenant → VM, sandbox id → {tenant, workdir}.

use super::{apply_system_base, build_spec};
use crate::manager::{SandboxRecord, VmManager};
use adapter_traits::VmName;
use terrarium_protocol::{Command, Response};

/// Fresh sandbox id: `sb-<8 hex>` (uuid v4, no new dependency).
fn new_sandbox_id() -> String {
    format!("sb-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn record_json(r: &SandboxRecord) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "tenant": r.tenant,
        "vm": r.vm_name,
        "workdir": r.workdir,
        "created_at": r.created_at,
    })
}

/// {"command":"sandbox_create","tenant":"research", kernel/layers/...}
///
/// Idempotently ensures the tenant VM exists (VM-spec fields mirror
/// `create`), then allocates a sandbox and creates its workdir in the
/// guest. Response data: {id, vm, workdir}.
pub(crate) async fn cmd_sandbox_create(mgr: &mut VmManager, cmd: Command) -> Response {
    let tenant = match cmd.tenant.clone() {
        Some(t) => t,
        None => return Response::err("Missing 'tenant' field"),
    };
    // Same whitelist as VmName — the tenant is embedded in the VM name.
    if let Err(e) = VmName::new(tenant.clone()) {
        return Response::err(format!("invalid tenant: {}", e));
    }
    let vm_name = format!("tenant-{}", tenant);

    // Reuse an already-registered tenant VM (ignore the spec), else create
    // it via the same path as `create`.
    if mgr.get(&vm_name).is_none() {
        let mut cmd = cmd;
        apply_system_base(&mut cmd);
        cmd.name = Some(vm_name.clone());
        let spec = match build_spec(&cmd) {
            Ok(s) => s,
            Err(e) => return Response::err(e),
        };
        if let Err(e) = spec.validate() {
            return Response::err(e);
        }
        if let Err(e) = mgr.spawn(spec).await {
            return Response::err(e.to_string());
        }
    }

    let id = new_sandbox_id();
    let workdir = format!("/workdir/{}", id);

    // Ensure the workdir exists in the guest (unsandboxed). On failure
    // return an honest error and don't register a half-created sandbox.
    let mkdir = vec!["mkdir".to_string(), "-p".to_string(), workdir.clone()];
    match mgr.exec(&vm_name, &mkdir, 30, false, None).await {
        Ok(r) if r.exit_code == 0 => {}
        Ok(r) => {
            return Response::err(format!(
                "failed to create workdir {}: {}",
                workdir,
                r.stderr.trim()
            ));
        }
        Err(e) => {
            return Response::err(format!("failed to create workdir {}: {}", workdir, e));
        }
    }

    let record = SandboxRecord {
        id: id.clone(),
        tenant,
        vm_name: vm_name.clone(),
        workdir: workdir.clone(),
        created_at: now_secs(),
    };
    mgr.sandbox_insert(record);
    Response::ok(serde_json::json!({
        "id": id,
        "vm": vm_name,
        "workdir": workdir,
    }))
}

/// {"command":"sandbox_exec","id":"sb-...","args":[...],"timeout_secs":N,
///  "sandbox":bool,"exec_mode":...}
///
/// Runs in the tenant VM with cwd = the sandbox workdir. Confinement
/// defaults to ON (absent `sandbox` → true).
pub(crate) async fn cmd_sandbox_exec(mgr: &mut VmManager, cmd: Command) -> Response {
    let id = match cmd.id.clone() {
        Some(i) => i,
        None => return Response::err("Missing 'id' field"),
    };
    let record = match mgr.sandbox_get(&id) {
        Some(r) => r,
        None => return Response::err(format!("Sandbox '{}' not found", id)),
    };
    if cmd.args.is_empty() {
        return Response::err("Missing 'args' field");
    }
    let timeout = cmd.timeout_secs.unwrap_or(60).min(3600);
    let sandbox = cmd.sandbox.unwrap_or(true);

    let mode = cmd.exec_mode.as_deref().unwrap_or("blocking");
    match mode {
        "background" => {
            let session_id = uuid::Uuid::new_v4().to_string();
            match mgr
                .exec_background(
                    &record.vm_name,
                    &cmd.args,
                    timeout,
                    sandbox,
                    &session_id,
                    Some(&record.workdir),
                    Some(id.clone()),
                )
                .await
            {
                Ok(()) => Response::ok(serde_json::json!({
                    "session_id": session_id,
                    "sandbox": id,
                    "status": "started",
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }
        "blocking" => {
            match mgr
                .exec(
                    &record.vm_name,
                    &cmd.args,
                    timeout,
                    sandbox,
                    Some(&record.workdir),
                )
                .await
            {
                Ok(r) => Response::ok(serde_json::json!({
                    "stdout": r.stdout,
                    "stderr": r.stderr,
                    "exit_code": r.exit_code,
                })),
                Err(e) => Response::err(e.to_string()),
            }
        }
        other => Response::err(format!(
            "invalid exec_mode {:?}: expected \"blocking\" or \"background\"",
            other
        )),
    }
}

/// {"command":"sandbox_list","tenant"?}
pub(crate) fn cmd_sandbox_list(mgr: &VmManager, cmd: Command) -> Response {
    let records = mgr.sandbox_list(cmd.tenant.as_deref());
    let items: Vec<_> = records.iter().map(record_json).collect();
    Response::ok(serde_json::json!({
        "sandboxes": items,
        "count": items.len(),
    }))
}

/// {"command":"sandbox_info","id":"sb-..."}
pub(crate) fn cmd_sandbox_info(mgr: &VmManager, cmd: Command) -> Response {
    let id = match cmd.id {
        Some(i) => i,
        None => return Response::err("Missing 'id' field"),
    };
    match mgr.sandbox_get(&id) {
        Some(r) => Response::ok(record_json(&r)),
        None => Response::err(format!("Sandbox '{}' not found", id)),
    }
}

/// {"command":"sandbox_kill","id":"sb-..."}
///
/// Kills every live session of this sandbox, removes the workdir from the
/// guest, and drops the record. The shared tenant VM keeps running.
pub(crate) async fn cmd_sandbox_kill(mgr: &mut VmManager, cmd: Command) -> Response {
    let id = match cmd.id.clone() {
        Some(i) => i,
        None => return Response::err("Missing 'id' field"),
    };
    let record = match mgr.sandbox_get(&id) {
        Some(r) => r,
        None => return Response::err(format!("Sandbox '{}' not found", id)),
    };

    // Kill every live session registered for this sandbox.
    let live: Vec<_> = mgr
        .session_list()
        .into_iter()
        .filter(|s| s.sandbox.as_deref() == Some(id.as_str()) && s.status == "running")
        .collect();
    for s in &live {
        if let Err(e) = mgr.session_kill(&s.session_id).await {
            return Response::err(format!("failed to kill session '{}': {}", s.session_id, e));
        }
    }

    // Untrusted-input discipline: only ever rm a path we created.
    if !record.workdir.starts_with("/workdir/sb-") {
        return Response::err(format!(
            "refusing to remove unexpected workdir path {:?}",
            record.workdir
        ));
    }
    let rm = vec!["rm".to_string(), "-rf".to_string(), record.workdir.clone()];
    match mgr.exec(&record.vm_name, &rm, 30, false, None).await {
        Ok(r) if r.exit_code == 0 => {}
        Ok(r) => {
            return Response::err(format!(
                "failed to remove workdir {}: {}",
                record.workdir,
                r.stderr.trim()
            ));
        }
        Err(e) => {
            return Response::err(format!(
                "failed to remove workdir {}: {}",
                record.workdir, e
            ));
        }
    }

    mgr.sandbox_remove(&id);
    Response::ok(serde_json::json!({
        "id": id,
        "sessions_killed": live.len(),
        "status": "killed",
    }))
}

/// {"command":"tenant_destroy","tenant":"research"}
///
/// Destroys the tenant VM (same semantics as `destroy`) and drops all of
/// the tenant's sandbox records.
pub(crate) async fn cmd_tenant_destroy(mgr: &mut VmManager, cmd: Command) -> Response {
    let tenant = match cmd.tenant {
        Some(t) => t,
        None => return Response::err("Missing 'tenant' field"),
    };
    if let Err(e) = VmName::new(tenant.clone()) {
        return Response::err(format!("invalid tenant: {}", e));
    }
    let vm_name = format!("tenant-{}", tenant);
    match mgr.destroy(&vm_name).await {
        Ok(()) => {
            let removed = mgr.sandbox_remove_tenant(&tenant);
            Response::ok(serde_json::json!({
                "tenant": tenant,
                "vm": vm_name,
                "sandboxes_removed": removed,
                "status": "destroyed",
            }))
        }
        Err(e) => Response::err(e.to_string()),
    }
}

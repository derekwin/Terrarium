use super::{apply_system_base, build_spec, require_name};
use crate::manager::VmManager;
use terrarium_protocol::{Command, Response};

pub(crate) async fn cmd_create(mgr: &mut VmManager, cmd: Command) -> Response {
    // The system base is implicit: tool layers stack on top of it.
    let mut cmd = cmd;
    apply_system_base(&mut cmd);
    let spec = match build_spec(&cmd) {
        Ok(s) => s,
        Err(e) => return Response::err(e),
    };
    if let Err(e) = spec.validate() {
        return Response::err(e);
    }
    let name = spec.name.to_string();
    match mgr.spawn(spec).await {
        Ok(()) => {
            let pid = mgr.get(&name).map(|h| h.pid());
            Response::ok(serde_json::json!({"name": name, "status": "created", "pid": pid}))
        }
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) async fn cmd_list(mgr: &VmManager) -> Response {
    let names = mgr.list_names();
    let mut vms = Vec::new();
    for name in &names {
        if let Some(vm) = mgr.get(name) {
            let info = vm.info().await.ok();
            vms.push(serde_json::json!({
                "name": name,
                "pid": vm.pid(),
                "state": info.as_ref().map_or("unknown", |i| i.state.as_str()),
            }));
        }
    }
    Response::ok(serde_json::json!({"vms": vms, "count": vms.len()}))
}

pub(crate) async fn cmd_info(mgr: &VmManager, cmd: Command) -> Response {
    let (vm, name) = match super::get_vm(mgr, &cmd) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let details = match vm.info().await {
        Ok(d) => d,
        Err(e) => return Response::err(e.to_string()),
    };
    Response::ok(serde_json::json!({
        "name": name,
        "pid": vm.pid(),
        "state": details.state,
        "cpus": details.cpus,
        "memory_mb": details.memory_mb,
    }))
}

pub(crate) async fn cmd_resize(mgr: &mut VmManager, cmd: Command) -> Response {
    let (vm, name) = match super::get_vm(mgr, &cmd) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let cpus: Option<u32> = cmd.cpus.map(|c| c as u32);
    if cpus.is_none() && cmd.memory_bytes.is_none() {
        return Response::err(
            "At least one of 'cpus' or 'memory_bytes' must be specified for resize",
        );
    }
    // CPU shrink is not supported: CH vCPU removal requires guest-side
    // offlining, and guest-proxy only ever ONLINES hot-added vCPUs
    // (start_cpu_onliner). Reject explicitly instead of forwarding a
    // failing resize to CH. Memory shrink IS supported (virtio-mem; the
    // guest driver handles unplug) and is unaffected by this guard.
    if let Some(want) = cpus {
        // info() can fail while the VM is still booting; in that case we
        // fall through to the existing resize path so a memory resize is
        // never blocked by an unverifiable cpus comparison (CH decides,
        // as before). When cpus was requested, the same unverifiable
        // state lets CH error as it would today.
        if let Ok(info) = vm.info().await {
            if info.cpus.is_some_and(|cur| cur as u32 > want) {
                return Response::err(
                    "CPU shrink is not supported (hot-unplug requires guest offlining)",
                );
            }
        }
    }
    if let Err(e) = vm.resize(cpus, cmd.memory_bytes).await {
        return Response::err(e.to_string());
    }
    // Sync the recorded policy with the new allocation: it is the quota
    // sandbox limits are validated against (policy-model.md §3.5), so a
    // stale boot-time entry would reject sandboxes a grown VM can host
    // (and admit sandboxes a shrunk VM cannot). `max_*` ceilings stay.
    let memory_mb = cmd.memory_bytes.map(|b| b / 1024 / 1024);
    if let Err(e) = mgr.record_resize(&name, cpus, memory_mb) {
        return Response::err(e.to_string());
    }
    // VM-level resource governance is always auditable — a platform action
    // with no per-sandbox policy, so it is emitted unconditionally.
    crate::audit::audit_vm_resize(&name, cpus, cmd.memory_bytes);
    Response::ok_msg("resize completed")
}

pub(crate) async fn cmd_shutdown(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match require_name(&cmd) {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    match mgr.shutdown(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' shut down", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) async fn cmd_kill(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match require_name(&cmd) {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    match mgr.kill(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' killed", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) async fn cmd_destroy(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match require_name(&cmd) {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    match mgr.destroy(&name).await {
        Ok(()) => {
            // Snapshot artifacts of this VM are garbage — snapshot is a
            // platform extension (not an agent contract) with no state
            // reload consumer yet. Best-effort cleanup.
            // Path scheme: the CH adapter produces
            // {snapshot_dir}/terra-snap-{name}.bin (its snapshot() forms
            // /tmp/terra-snap-{name}.bin; snapshot_dir is "/tmp" in the
            // daemon) and Cloud Hypervisor writes a sibling
            // {snapshot_dir}/terra-snap-{name}.mem — cleanup must mirror
            // both, so the .mem sibling never leaks.
            for p in [
                format!("{}/terra-snap-{}.bin", mgr.snapshot_dir(), name),
                format!("{}/terra-snap-{}.mem", mgr.snapshot_dir(), name),
            ] {
                if std::fs::remove_file(&p).is_ok() {
                    tracing::info!(path = %p, "removed snapshot artifact");
                }
            }
            Response::ok_msg(&format!("VM '{}' destroyed", name))
        }
        Err(e) => Response::err(e.to_string()),
    }
}

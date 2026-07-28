use super::{build_spec, SYSTEM_BASES};
use crate::manager::VmManager;
use terrarium_protocol::{Command, Response};

pub(crate) async fn cmd_create(mgr: &mut VmManager, cmd: Command) -> Response {
    // The system base is implicit: tool layers stack on top of it.
    // Append it unless the caller already ended the list with one.
    let mut cmd = cmd;
    if !cmd.layers.is_empty() {
        let last = cmd.layers.last().map(|s| s.as_str()).unwrap_or("");
        if !SYSTEM_BASES.contains(&last) {
            let system = cmd.system.clone().unwrap_or_else(|| "base".into());
            cmd.layers.push(system);
        }
    }
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
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    let vm = match mgr.get(&name) {
        Some(v) => v,
        None => return Response::err(format!("VM '{}' not found", name)),
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

pub(crate) async fn cmd_resize(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    let vm = match mgr.get(&name) {
        Some(v) => v,
        None => return Response::err(format!("VM '{}' not found", name)),
    };

    let cpus: Option<u32> = cmd.cpus.map(|c| c as u32);
    if cpus.is_none() && cmd.memory_bytes.is_none() {
        return Response::err(
            "At least one of 'cpus' or 'memory_bytes' must be specified for resize",
        );
    }
    if let Err(e) = vm.resize(cpus, cmd.memory_bytes).await {
        return Response::err(e.to_string());
    }
    Response::ok_msg("resize completed")
}

pub(crate) async fn cmd_shutdown(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.shutdown(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' shut down", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) async fn cmd_kill(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.kill(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' killed", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) async fn cmd_destroy(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.destroy(&name).await {
        Ok(()) => {
            // Snapshot artifacts of this VM are garbage until restore
            // lands (then this becomes opt-in). Best-effort cleanup.
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

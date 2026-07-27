//! Command execution — shared by daemon and CLI modes.
//!
//! Each function takes a `&mut VmManager` and a typed command payload,
//! executes it, and returns a serializable response.

use crate::manager::VmManager;
use adapter_traits::{VmName, VmSpec};
pub use terrarium_protocol::{Command, Response};

/// Execute a command against the given VM manager.
pub async fn execute(mgr: &mut VmManager, cmd: Command) -> Response {
    match cmd.command.as_str() {
        // VM commands: compute lifecycle only — never touch disks.
        "create" => cmd_create(mgr, cmd).await,
        "list" => cmd_list(mgr).await,
        "info" => cmd_info(mgr, cmd).await,
        "resize" => cmd_resize(mgr, cmd).await,
        "shutdown" => cmd_shutdown(mgr, cmd).await,
        "kill" => cmd_kill(mgr, cmd).await,
        "destroy" => cmd_destroy(mgr, cmd).await,
        "snapshot" => cmd_snapshot(mgr, cmd).await,
        "restore" => cmd_restore(mgr, cmd),
        "attach_fs" => cmd_attach_fs(mgr, cmd).await,
        "detach_fs" => cmd_detach_fs(mgr, cmd).await,
        _ => Response::err(format!("Unknown command: {}", cmd.command)),
    }
}

fn build_spec(cmd: &Command) -> Result<VmSpec, String> {
    let name = cmd.name.as_ref().ok_or("Missing 'name' field")?;
    let kernel = cmd.kernel.as_ref().ok_or("Missing 'kernel' field")?;

    let vm_name = VmName::new(name.clone())?;
    let boot_vcpus = cmd.cpus.unwrap_or(2);
    let max_vcpus = cmd.max_cpus;
    let memory_mb = cmd.memory_mb.unwrap_or(512);
    let max_memory_mb = cmd
        .max_memory_mb
        .or_else(|| cmd.hotplug_memory_gb.map(|gb| gb * 1024));

    Ok(VmSpec {
        name: vm_name,
        kernel: kernel.clone(),
        cmdline: cmd.cmdline.clone(),
        boot_vcpus,
        max_vcpus,
        memory_mb,
        max_memory_mb,
        initramfs: cmd.initramfs.clone(),
        fs: if cmd.layers.is_empty() {
            None
        } else {
            Some(adapter_traits::FsSpec {
                layers: cmd.layers.clone(),
                upper: adapter_traits::UpperPolicy::Ephemeral,
            })
        },
        backend_config: None,
    })
}

async fn cmd_create(mgr: &mut VmManager, cmd: Command) -> Response {
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

async fn cmd_list(mgr: &VmManager) -> Response {
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

async fn cmd_info(mgr: &VmManager, cmd: Command) -> Response {
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

async fn cmd_resize(mgr: &VmManager, cmd: Command) -> Response {
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

async fn cmd_shutdown(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.shutdown(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' shut down", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

async fn cmd_kill(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.kill(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' killed", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

async fn cmd_destroy(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.destroy(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' destroyed", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

async fn cmd_snapshot(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    let vm = match mgr.get(&name) {
        Some(v) => v,
        None => return Response::err(format!("VM '{}' not found", name)),
    };
    let _path = cmd
        .snapshot_path
        .unwrap_or_else(|| format!("/tmp/terra-snap-{}.bin", name));

    match vm.snapshot().await {
        Ok(snap) => Response::ok(serde_json::json!({"snapshot_path": snap.path})),
        Err(e) => Response::err(e.to_string()),
    }
}

fn cmd_restore(_mgr: &mut VmManager, _cmd: Command) -> Response {
    Response::err("restore not implemented")
}

async fn cmd_attach_fs(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name.as_deref() {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    if cmd.layers.is_empty() {
        return Response::err("Missing 'layers' field");
    }
    let fs = adapter_traits::FsSpec {
        layers: cmd.layers.clone(),
        upper: adapter_traits::UpperPolicy::Ephemeral,
    };
    match mgr.attach_fs(name, &fs).await {
        Ok(()) => Response::ok_msg(&format!("fs attached to VM '{}'", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

async fn cmd_detach_fs(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name.as_deref() {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.detach_fs(name).await {
        Ok(()) => Response::ok_msg(&format!("fs detached from VM '{}'", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

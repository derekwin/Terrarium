//! Command execution — shared by daemon and CLI modes.
//!
//! Each function takes a `&mut VmManager` and a typed command payload,
//! executes it, and returns a serializable response.

use serde::{Deserialize, Serialize};

use crate::manager::VmManager;
use adapter_traits::{VmName, VmSpec};

/// A command received from the client.
#[derive(Debug, Deserialize)]
pub struct Command {
    pub command: String,

    // create / info / resize / shutdown / kill
    #[serde(default)]
    pub name: Option<String>,

    // create
    #[serde(default)]
    pub kernel: Option<String>,
    #[serde(default)]
    pub initramfs: Option<String>,
    #[serde(default)]
    pub disks: Vec<String>,
    #[serde(default)]
    pub cmdline: Option<String>,
    #[serde(default)]
    pub cpus: Option<u8>,
    #[serde(default)]
    pub max_cpus: Option<u8>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    #[serde(default)]
    pub hotplug_memory_gb: Option<u64>,
    #[serde(default)]
    pub max_memory_mb: Option<u64>,

    // snapshot / restore
    #[serde(default)]
    pub snapshot_path: Option<String>,
    #[serde(default)]
    pub base_disk: Option<String>,
    #[serde(default)]
    pub disk_size_gb: Option<u64>,

    // resize
    #[serde(default)]
    pub memory_bytes: Option<u64>,
}

/// Response sent back to the client.
#[derive(Debug, Serialize)]
pub struct Response {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            status: "ok".into(),
            data: Some(data),
            error: None,
        }
    }

    pub fn ok_msg(msg: &str) -> Self {
        Self::ok(serde_json::json!({"message": msg}))
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self {
            status: "error".into(),
            data: None,
            error: Some(error.into()),
        }
    }
}

/// Execute a command against the given VM manager.
pub async fn execute(mgr: &mut VmManager, cmd: Command) -> Response {
    match cmd.command.as_str() {
        "create" => cmd_create(mgr, cmd).await,
        "list" => cmd_list(mgr).await,
        "info" => cmd_info(mgr, cmd).await,
        "resize" => cmd_resize(mgr, cmd).await,
        "shutdown" => cmd_shutdown(mgr, cmd).await,
        "kill" => cmd_kill(mgr, cmd).await,
        "destroy" => cmd_destroy(mgr, cmd).await,
        "snapshot" => cmd_snapshot(mgr, cmd).await,
        "restore" => cmd_restore(mgr, cmd),
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
        disks: cmd.disks.clone(),
        base_disk: cmd.base_disk.clone(),
        disk_size_gb: cmd.disk_size_gb.unwrap_or(20),
        backend_config: None,
    })
}

async fn cmd_create(mgr: &mut VmManager, cmd: Command) -> Response {
    let spec = match build_spec(&cmd) {
        Ok(s) => s,
        Err(e) => return Response::err(e),
    };
    let name = spec.name.to_string();
    match mgr.spawn(spec).await {
        Ok(()) => Response::ok(serde_json::json!({"name": name, "status": "created"})),
        Err(e) => Response::err(e),
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
        Err(e) => return Response::err(e),
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
    if let Err(e) = vm.resize(cpus, cmd.memory_bytes).await {
        return Response::err(e);
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
        Err(e) => Response::err(e),
    }
}

async fn cmd_kill(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.kill(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' killed", name)),
        Err(e) => Response::err(e),
    }
}

async fn cmd_destroy(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.destroy(&name).await {
        Ok(()) => Response::ok_msg(&format!("VM '{}' destroyed (disk removed)", name)),
        Err(e) => Response::err(e),
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
        Err(e) => Response::err(e),
    }
}

fn cmd_restore(_mgr: &mut VmManager, _cmd: Command) -> Response {
    Response::err("restore not implemented")
}

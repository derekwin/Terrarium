//! Command execution — shared by daemon and CLI modes.
//!
//! Each function takes a `&mut VmManager` and a typed command payload,
//! executes it, and returns a serializable response.

use serde::{Deserialize, Serialize};

use crate::manager::VmManager;
use crate::spec::VmSpec;

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
    #[serde(default)]
    pub ch_binary: Option<String>,

    // snapshot / restore
    #[serde(default)]
    pub snapshot_path: Option<String>,
    #[serde(default)]
    pub base_disk: Option<String>,
    #[serde(default)]
    pub tool_layers: Vec<String>,
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
pub fn execute(mgr: &mut VmManager, cmd: Command) -> Response {
    match cmd.command.as_str() {
        "create" => cmd_create(mgr, cmd),
        "list" => cmd_list(mgr),
        "info" => cmd_info(mgr, cmd),
        "resize" => cmd_resize(mgr, cmd),
        "shutdown" => cmd_shutdown(mgr, cmd),
        "kill" => cmd_kill(mgr, cmd),
        "destroy" => cmd_destroy(mgr, cmd),
        "snapshot" => cmd_snapshot(mgr, cmd),
        "restore" => cmd_restore(mgr, cmd),
        _ => Response::err(format!("Unknown command: {}", cmd.command)),
    }
}

fn cmd_create(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    let kernel = match cmd.kernel {
        Some(k) => k,
        None => return Response::err("Missing 'kernel' field"),
    };

    let mut spec = VmSpec::new(&name, kernel);
    if let Some(c) = cmd.cmdline {
        spec = spec.cmdline(c);
    }
    if let Some(c) = cmd.cpus {
        let max = cmd.max_cpus;
        spec = spec.cpus(c, max);
    }
    if let Some(m) = cmd.memory_mb {
        spec = spec.memory_mb(m);
    }
    if let Some(h) = cmd.hotplug_memory_gb {
        spec = spec.hotplug_memory_gb(h);
    }
    if let Some(m) = cmd.max_memory_mb {
        let boot_mb = spec.memory_mb;
        spec = spec.memory_range(boot_mb, Some(m));
    }
    if let Some(b) = cmd.ch_binary {
        spec = spec.ch_binary(b);
    } else if std::path::Path::new("/tmp/cloud-hypervisor-static").exists() {
        spec = spec.ch_binary("/tmp/cloud-hypervisor-static");
    }
    if let Some(i) = cmd.initramfs {
        spec = spec.initramfs(i);
    }
    for disk in &cmd.disks {
        spec = spec.disk(disk);
    }
    if let Some(ref b) = cmd.base_disk {
        spec = spec.base_disk(b.clone());
    }
    for tool in &cmd.tool_layers {
        spec = spec.tool_layer(tool);
    }
    if let Some(gb) = cmd.disk_size_gb {
        spec = spec.disk_size_gb(gb);
    }

    match mgr.spawn(spec) {
        Ok(handle) => {
            let info = handle.info().ok();
            Response::ok(serde_json::json!({
                "name": handle.name(),
                "pid": handle.pid(),
                "state": info.as_ref().map_or("unknown", |i| i.state.as_str()),
            }))
        }
        Err(e) => Response::err(e.to_string()),
    }
}

fn cmd_list(mgr: &VmManager) -> Response {
    let names = mgr.list_names();
    let vms: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            let vm = mgr.get(name).unwrap();
            let info = vm.info().ok();
            serde_json::json!({
                "name": name,
                "pid": vm.pid(),
                "state": info.as_ref().map_or("unknown", |i| i.state.as_str()),
            })
        })
        .collect();
    Response::ok(serde_json::json!({"vms": vms, "count": vms.len()}))
}

fn cmd_info(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    let vm = match mgr.get(&name) {
        Ok(v) => v,
        Err(e) => return Response::err(e.to_string()),
    };
    let details = match vm.info() {
        Ok(d) => d,
        Err(e) => return Response::err(e.to_string()),
    };
    Response::ok(serde_json::json!({
        "name": name,
        "pid": vm.pid(),
        "state": details.state,
        "cpus": details.config.as_ref().and_then(|c| c.cpus.as_ref()).map(|c| serde_json::json!({"boot": c.boot, "max": c.max})),
        "memory": details.memory_actual_size.or_else(|| details.config.as_ref().and_then(|c| c.memory.as_ref()).map(|m| m.size)),
    }))
}

fn cmd_resize(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    let vm = match mgr.get(&name) {
        Ok(v) => v,
        Err(e) => return Response::err(e.to_string()),
    };

    if let Some(c) = cmd.cpus {
        if let Err(e) = vm.resize_vcpus(Some(c)) {
            return Response::err(e.to_string());
        }
    }
    if let Some(m) = cmd.memory_bytes {
        if let Err(e) = vm.resize_memory(Some(m)) {
            return Response::err(e.to_string());
        }
    }
    Response::ok_msg("resize completed")
}

fn cmd_shutdown(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.shutdown(&name) {
        Ok(()) => Response::ok_msg(&format!("VM '{}' shut down", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

fn cmd_kill(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.kill(&name) {
        Ok(()) => Response::ok_msg(&format!("VM '{}' killed", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

fn cmd_destroy(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.destroy(&name) {
        Ok(()) => Response::ok_msg(&format!("VM '{}' destroyed (disk removed)", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

fn cmd_snapshot(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    let vm = match mgr.get(&name) {
        Ok(v) => v,
        Err(e) => return Response::err(e.to_string()),
    };
    let path = cmd
        .snapshot_path
        .unwrap_or_else(|| format!("/tmp/terra-snap-{}.bin", name));

    match crate::vm::snapshot_vm(vm.client(), &path) {
        Ok(()) => Response::ok(serde_json::json!({"snapshot_path": path})),
        Err(e) => Response::err(e.to_string()),
    }
}

fn cmd_restore(mgr: &mut VmManager, cmd: Command) -> Response {
    let path = match cmd.snapshot_path {
        Some(p) => p,
        None => return Response::err("Missing 'snapshot_path' field"),
    };
    let name = cmd.name.unwrap_or_else(|| "restored".to_string());
    let kernel = cmd
        .kernel
        .unwrap_or_else(|| "target/guest/vmlinux.bin".to_string());

    let mut spec = VmSpec::new(&name, kernel);
    eprintln!("DEBUG: base_disk in cmd = {:?}", cmd.base_disk);
    if let Some(ref b) = cmd.base_disk {
        spec = spec.base_disk(b.clone());
    }
    eprintln!("DEBUG: spec.base_disk = {:?}", spec.base_disk);
    if let Some(gb) = cmd.disk_size_gb {
        spec = spec.disk_size_gb(gb);
    }

    match mgr.spawn(spec) {
        Ok(_handle) => Response::ok_msg(&format!(
            "VM '{}' spawned (restore from {} requires warm-pool manager)",
            name, path
        )),
        Err(e) => Response::err(e.to_string()),
    }
}

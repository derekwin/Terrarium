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
        "exec" => cmd_exec(mgr, cmd).await,
        "net_list" => cmd_net_list(mgr),
        "net_down" => cmd_net_down(mgr),
        "pool_create" => cmd_pool_create(mgr, cmd).await,
        "pool_list" => cmd_pool_list(mgr),
        "pool_claim" => cmd_pool_claim(mgr, cmd).await,
        "pool_release" => cmd_pool_release(mgr, cmd).await,
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
        Ok(()) => {
            // Snapshot artifacts of this VM are garbage until restore
            // lands (then this becomes opt-in). Best-effort cleanup.
            for p in [
                format!("/tmp/terra-snap-{}.bin", name),
                format!("/tmp/terra-snap-{}.mem", name),
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

async fn cmd_exec(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name.as_deref() {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    if cmd.args.is_empty() {
        return Response::err("Missing 'args' field");
    }
    let timeout = cmd.timeout_secs.unwrap_or(60).min(3600);
    match mgr.exec(name, &cmd.args, timeout).await {
        Ok(r) => Response::ok(serde_json::json!({
            "stdout": r.stdout,
            "stderr": r.stderr,
            "exit_code": r.exit_code,
        })),
        Err(e) => Response::err(e.to_string()),
    }
}

fn cmd_net_down(mgr: &VmManager) -> Response {
    let in_use = mgr.net_in_use();
    if in_use > 0 {
        return Response::err(format!(
            "{} VM(s) still using the bridge — destroy them first",
            in_use
        ));
    }
    match terrarium_network::teardown_nat_bridge(
        terrarium_network::DEFAULT_BRIDGE,
        terrarium_network::DEFAULT_GATEWAY,
        terrarium_network::DEFAULT_PREFIX,
    ) {
        Ok(()) => Response::ok_msg("NAT bridge, DHCP, and masquerade removed"),
        Err(e) => Response::err(e),
    }
}

fn cmd_net_list(mgr: &VmManager) -> Response {
    let vms: Vec<_> = mgr
        .list_names()
        .into_iter()
        .filter(|n| mgr.has_net(n))
        .map(|n| {
            let tap = format!(
                "terra-{}",
                n.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .take(9)
                    .collect::<String>()
            );
            serde_json::json!({"name": n, "tap": tap, "bridge": "terra0"})
        })
        .collect();
    Response::ok(serde_json::json!({
        "bridge": "terra0",
        "gateway": "10.200.0.1/24",
        "mode": "nat",
        "vms": vms,
    }))
}

async fn cmd_pool_create(mgr: &mut VmManager, cmd: Command) -> Response {
    let size = cmd.pool_size.unwrap_or(1);
    if size == 0 || size > 32 {
        return Response::err("pool_size must be between 1 and 32");
    }
    let kernel = cmd.kernel.clone().unwrap_or_else(|| {
        std::env::var("TERRA_KERNEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "target/guest/vmlinux.bin".into())
    });
    let agent = std::env::var("TERRA_AGENT_INITRAMFS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "target/guest/initramfs-agent.cpio.gz".into());
    match mgr.pool_create(size, &kernel, &agent, cmd.net).await {
        Ok(names) => Response::ok(serde_json::json!({"created": names, "count": names.len()})),
        Err(e) => Response::err(e.to_string()),
    }
}

fn cmd_pool_list(mgr: &VmManager) -> Response {
    let slots: Vec<_> = mgr
        .pool_list()
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "claimed": s.claimed,
                "layers": s.layers,
            })
        })
        .collect();
    Response::ok(serde_json::json!({"pool": slots, "count": slots.len()}))
}

async fn cmd_pool_claim(mgr: &mut VmManager, cmd: Command) -> Response {
    if cmd.layers.is_empty() {
        return Response::err("Missing 'layers' field");
    }
    match mgr.pool_claim(cmd.layers.clone()).await {
        Ok(name) => Response::ok(serde_json::json!({
            "name": name,
            "pid": mgr.get(&name).map(|h| h.pid()),
            "layers": cmd.layers,
        })),
        Err(e) => Response::err(e.to_string()),
    }
}

async fn cmd_pool_release(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match cmd.name.as_deref() {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.pool_release(name).await {
        Ok(()) => Response::ok_msg(&format!("pool VM '{}' released", name)),
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

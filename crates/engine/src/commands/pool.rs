use super::require_name;
use crate::manager::VmManager;
use adapter_traits::{VmName, VmSpec};
use terrarium_protocol::{Command, Response};

pub(crate) async fn cmd_pool_create(mgr: &mut VmManager, cmd: Command) -> Response {
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
        Ok(outcome) if outcome.failed.is_empty() => Response::ok(
            serde_json::json!({"created": outcome.ready, "count": outcome.ready.len()}),
        ),
        // Honest partial failure: report which VMs never became ready.
        Ok(outcome) if !outcome.ready.is_empty() => {
            let failed: Vec<_> = outcome
                .failed
                .iter()
                .map(|(name, err)| serde_json::json!({"name": name, "error": err}))
                .collect();
            Response::ok(serde_json::json!({
                "created": outcome.ready,
                "count": outcome.ready.len(),
                "failed": failed,
            }))
        }
        Ok(outcome) => Response::err(format!(
            "no pool VM became ready: {}",
            outcome
                .failed
                .iter()
                .map(|(n, e)| format!("{}: {}", n, e))
                .collect::<Vec<_>>()
                .join("; ")
        )),
        Err(e) => Response::err(e.to_string()),
    }
}

/// Plan for a lock-free READY-pool fill: the specs are built under the
/// manager lock, the restores + agent pings run outside it (in parallel),
/// and the re-lock registers each slot.
pub(crate) struct PoolSnapshotPlan {
    pub specs: Vec<VmSpec>,
    pub snapshot: adapter_traits::Snapshot,
    pub layers: Vec<String>,
    pub net: bool,
}

/// Validate + build the restore specs (cheap, under the lock). Assigns
/// pool-N names from the manager's counter so parallel restores never
/// collide with a concurrent pool_create.
pub(crate) fn prepare_pool_create_snapshot(
    mgr: &mut VmManager,
    cmd: &Command,
) -> Result<PoolSnapshotPlan, Response> {
    let size = cmd.pool_size.unwrap_or(1);
    if size == 0 || size > 32 {
        return Err(Response::err("pool_size must be between 1 and 32"));
    }
    let snapshot_path = match cmd.snapshot_path.clone() {
        Some(p) if !p.is_empty() => p,
        _ => return Err(Response::err("Missing 'snapshot_path' field")),
    };
    let kernel = cmd.kernel.clone().unwrap_or_else(|| {
        std::env::var("TERRA_KERNEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "target/guest/vmlinux.bin".into())
    });
    let layers = cmd.layers.clone();
    if layers.is_empty() {
        return Err(Response::err("Missing 'layers' field"));
    }
    let memory_mb = cmd.memory_mb.unwrap_or(256);
    let snapshot = adapter_traits::Snapshot {
        path: snapshot_path,
    };
    let mut specs = Vec::with_capacity(size as usize);
    for _ in 0..size {
        let name = format!("pool-{}", mgr.pool_next_id);
        mgr.pool_next_id += 1;
        let vm_name = VmName::new(name.clone())
            .map_err(|e| Response::err(format!("invalid pool name: {}", e)))?;
        specs.push(VmSpec {
            name: vm_name,
            kernel: Some(kernel.clone()),
            cmdline: None,
            boot_vcpus: 1,
            max_vcpus: Some(4),
            memory_mb,
            max_memory_mb: Some(1024),
            initramfs: None,
            net: cmd.net,
            fs: Some(adapter_traits::FsSpec {
                layers: layers.clone(),
                upper: adapter_traits::UpperPolicy::Ephemeral,
            }),
        });
    }
    Ok(PoolSnapshotPlan {
        specs,
        snapshot,
        layers,
        net: cmd.net,
    })
}

/// Run the restores in PARALLEL (the slow part), then wait for each
/// restored VM's guest agent with a bounded ping. Lock-free.
pub(crate) async fn finish_pool_create_snapshot(
    plan: &PoolSnapshotPlan,
    adapter: std::sync::Arc<dyn adapter_traits::VmAdapter>,
) -> Vec<(
    VmSpec,
    Option<(Box<dyn adapter_traits::VmHandle>, Result<(), String>)>,
)> {
    // Restore all VMs in PARALLEL (the slow part); the agent ping for each
    // runs on its own task so the waits overlap too.
    let mut set = tokio::task::JoinSet::new();
    for spec in &plan.specs {
        let adapter = adapter.clone();
        let snapshot = plan.snapshot.clone();
        let spec = spec.clone();
        set.spawn(async move { adapter.restore(&snapshot, &spec).await });
    }
    let mut restored: Vec<Result<Box<dyn adapter_traits::VmHandle>, String>> = Vec::new();
    while let Some(res) = set.join_next().await {
        restored.push(res.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string())));
    }
    let mut out = Vec::with_capacity(plan.specs.len());
    for (spec, handle) in plan.specs.iter().zip(restored) {
        match handle {
            Ok(h) => {
                // bounded agent ping (the snapshot's agent resumes quickly)
                let mut ping: Result<(), adapter_traits::AdapterError> =
                    Err(adapter_traits::AdapterError::internal("not pinged".to_string()));
                for _ in 0..20 {
                    ping = h.ping().await;
                    if ping.is_ok() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                out.push((spec.clone(), Some((h, ping.map_err(|e| e.to_string())))));
            }
            Err(_e) => out.push((spec.clone(), None)),
        }
    }
    out
}

/// Shared response builder for the READY-pool fill outcome.
pub(crate) fn pool_create_snapshot_response(
    ready: Vec<String>,
    failed: Vec<(String, String)>,
) -> Response {
    if failed.is_empty() {
        Response::ok(serde_json::json!({
            "created": ready,
            "count": ready.len(),
            "ready": true,
        }))
    } else if !ready.is_empty() {
        let failed_json: Vec<_> = failed
            .iter()
            .map(|(name, err)| serde_json::json!({"name": name, "error": err}))
            .collect();
        Response::ok(serde_json::json!({
            "created": ready,
            "count": ready.len(),
            "failed": failed_json,
            "ready": true,
        }))
    } else {
        Response::err(format!(
            "no ready pool VM became ready: {}",
            failed
                .iter()
                .map(|(n, e)| format!("{}: {}", n, e))
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

/// Locked execute() path (tests + non-daemon callers): restore each spec
/// under the lock, ping, register the READY slot.
pub(crate) async fn cmd_pool_create_snapshot(mgr: &mut VmManager, cmd: Command) -> Response {
    let plan = match prepare_pool_create_snapshot(mgr, &cmd) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let adapter = mgr.adapter();
    let results = finish_pool_create_snapshot(&plan, adapter).await;
    let mut ready = Vec::new();
    let mut failed = Vec::new();
    for (spec, result) in results {
        match result {
            Some((handle, Ok(()))) => {
                match mgr.pool_register_ready(&spec, handle, plan.layers.clone(), plan.net) {
                    Ok(()) => ready.push(spec.name.to_string()),
                    Err(e) => failed.push((spec.name.to_string(), e.to_string())),
                }
            }
            Some((handle, Err(e))) => {
                drop(handle); // teardown an agent-unready restored VM
                failed.push((spec.name.to_string(), e));
            }
            None => failed.push((spec.name.to_string(), "restore failed".to_string())),
        }
    }
    pool_create_snapshot_response(ready, failed)
}

pub(crate) fn cmd_pool_list(mgr: &VmManager) -> Response {
    let slots: Vec<_> = mgr
        .pool_list()
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "claimed": s.claimed,
                "layers": s.layers,
                "net": s.net,
                "ready": s.ready,
            })
        })
        .collect();
    Response::ok(serde_json::json!({"pool": slots, "count": slots.len()}))
}

pub(crate) async fn cmd_pool_claim(mgr: &mut VmManager, cmd: Command) -> Response {
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

pub(crate) async fn cmd_pool_release(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match require_name(&cmd) {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    match mgr.pool_release(&name).await {
        Ok(()) => Response::ok_msg(&format!("pool VM '{}' released", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) async fn cmd_pool_shrink(mgr: &mut VmManager, cmd: Command) -> Response {
    let count = cmd.pool_size.unwrap_or(1);
    let removed = mgr.pool_shrink(count).await;
    Response::ok(serde_json::json!({"removed": removed, "count": removed.len()}))
}

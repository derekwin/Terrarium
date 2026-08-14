use super::require_name;
use crate::manager::VmManager;
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

/// {"command":"pool_create_snapshot","size":N,"snapshot_path":<dir>,
///  "kernel":<path>,"layers":[...],"net":bool,"memory_mb":N}
///
/// Fill the pool with READY slots: VMs pre-restored from a snapshot with
/// the layered fs attached and the guest agent running. A subsequent
/// `sandbox_create` (pool=True) claims one with a direct sandbox bind —
/// no boot, no fs hot-plug. Releasing a slot resets it in place; the
/// ready state must live in the layer.
pub(crate) async fn cmd_pool_create_snapshot(mgr: &mut VmManager, cmd: Command) -> Response {
    let size = cmd.pool_size.unwrap_or(1);
    if size == 0 || size > 32 {
        return Response::err("pool_size must be between 1 and 32");
    }
    let snapshot_path = match cmd.snapshot_path.clone() {
        Some(p) if !p.is_empty() => p,
        _ => return Response::err("Missing 'snapshot_path' field"),
    };
    let kernel = cmd.kernel.clone().unwrap_or_else(|| {
        std::env::var("TERRA_KERNEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "target/guest/vmlinux.bin".into())
    });
    let layers = cmd.layers.clone();
    if layers.is_empty() {
        return Response::err("Missing 'layers' field");
    }
    let memory_mb = cmd.memory_mb.unwrap_or(256);
    let snapshot = adapter_traits::Snapshot {
        path: snapshot_path,
    };
    match mgr
        .pool_create_ready(size, &snapshot, &kernel, layers, cmd.net, memory_mb)
        .await
    {
        Ok(outcome) if outcome.failed.is_empty() => Response::ok(serde_json::json!({
            "created": outcome.ready,
            "count": outcome.ready.len(),
            "ready": true,
        })),
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
                "ready": true,
            }))
        }
        Ok(outcome) => Response::err(format!(
            "no ready pool VM became ready: {}",
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

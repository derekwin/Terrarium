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

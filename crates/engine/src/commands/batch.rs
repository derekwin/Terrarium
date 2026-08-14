//! Batch lifecycle API (P1 #2): create / exec / recycle N environments in
//! one command instead of N client round-trips — the RL/density cadence
//! ("一次拉起/回收数百环境"). All slow work (spawns, binds, execs,
//! resets/teardowns) runs on a JoinSet OUTSIDE the manager lock; the lock
//! is held only for registry resolution and final bookkeeping.

use std::sync::Arc;

use super::sandbox::{
    bind_sandbox, finalize_tenant_destroy, finish_sandbox_create, finish_tenant_destroy,
    prepare_sandbox_create, prepare_tenant_destroy, PreparedSandboxCreate, TenantDestroyPlan,
    TenantVm,
};
use super::{
    handle_executed_policy, prepared_exec_audited, sandbox_exec_command, validate_exec_quota,
    PreparedExec,
};
use crate::manager::{SandboxRecord, VmManager};
use adapter_traits::{SandboxPolicy, VmHandle};
use terrarium_protocol::{Command, Response};
use std::collections::HashMap;

const BATCH_MAX: u32 = 256;

fn batch_count(cmd: &Command) -> Result<usize, Response> {
    let n = cmd.count.unwrap_or(1);
    if n == 0 || n > BATCH_MAX {
        return Err(Response::err(format!(
            "count must be between 1 and {BATCH_MAX}"
        )));
    }
    Ok(n as usize)
}

// ── batch_create ─────────────────────────────────────────────────────────

/// One environment of a batch: the tenant name plus its prepared sandbox
/// (tenant VM resolved — ready-pool claim or cold spec — plus allocated
/// sandbox id + workdir). The daemon spawns + binds outside the lock.
pub(crate) struct BatchEnv {
    pub tenant: String,
    pub plan: PreparedSandboxCreate,
}

/// Validate + build N sandbox plans under the manager lock. Ready-pool
/// slots are claimed here (fast); the shortfall resolves to cold-boot
/// specs — size the ready pool first for fast fills.
pub(crate) async fn prepare_batch_create(
    mgr: &mut VmManager,
    cmd: &Command,
) -> Result<Vec<BatchEnv>, Response> {
    let count = batch_count(cmd)?;
    let prefix = cmd
        .prefix
        .clone()
        .ok_or_else(|| Response::err("Missing 'prefix' field"))?;
    if prefix.is_empty() || prefix.len() > 48 {
        return Err(Response::err("prefix must be 1..=48 chars"));
    }
    if cmd.layers.is_empty() {
        return Err(Response::err("Missing 'layers' field"));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let tenant = format!("{prefix}-{i}");
        let mut env_cmd = cmd.clone();
        env_cmd.tenant = Some(tenant.clone());
        let plan = prepare_sandbox_create(mgr, &env_cmd).await?;
        out.push(BatchEnv { tenant, plan });
    }
    Ok(out)
}

/// Spawn every cold-spec env in parallel (ready/existing envs need no
/// spawn). Returns (plan index, handle) pairs; the daemon registers them
/// on re-lock and binds afterwards.
pub(crate) async fn spawn_batch_vms(
    envs: &[BatchEnv],
    adapter: Arc<dyn adapter_traits::VmAdapter>,
) -> Vec<(usize, Box<dyn VmHandle>)> {
    let mut set = tokio::task::JoinSet::new();
    for (idx, env) in envs.iter().enumerate() {
        if let TenantVm::ColdSpec { spec, .. } = &env.plan.vm {
            let adapter = adapter.clone();
            let spec = spec.clone();
            set.spawn(async move {
                let r = adapter.create(&spec).await;
                (idx, r)
            });
        }
    }
    let mut spawned = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok((idx, Ok(handle))) = res {
            spawned.push((idx, handle));
        }
    }
    spawned
}

/// Bind all envs in parallel (L2 session + workdir). Returns per-env
/// results; failures are reported per-env, never fatal to the batch.
pub(crate) async fn bind_batch_envs(
    envs: &[BatchEnv],
    handles: &std::collections::HashMap<usize, Arc<dyn VmHandle>>,
    sb_adapter: Arc<dyn adapter_traits::SandboxAdapter>,
) -> Vec<(usize, Result<SandboxRecord, String>)> {
    let mut set = tokio::task::JoinSet::new();
    for (idx, env) in envs.iter().enumerate() {
        let handle: Option<Arc<dyn VmHandle>> = match &env.plan.vm {
            TenantVm::Existing { handle, .. } => Some(handle.clone()),
            TenantVm::ColdSpec { .. } => handles.get(&idx).cloned(),
        };
        let sb_adapter = sb_adapter.clone();
        let plan = env.plan.clone();
        set.spawn(async move {
            let Some(handle) = handle else {
                return (idx, Err("spawn failed".to_string()));
            };
            let r = bind_sandbox(&*sb_adapter, handle, &plan)
                .await
                .map_err(|resp| {
                    resp.error
                        .unwrap_or_else(|| "sandbox bind failed".to_string())
                });
            (idx, r)
        });
    }
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok((idx, r)) = res {
            out.push((idx, r));
        }
    }
    out
}

// ── batch_exec ───────────────────────────────────────────────────────────

/// One sandboxed exec of a batch, resolved under the lock; the daemon runs
/// `prepared_exec_audited` on it outside the lock.
pub(crate) struct BatchExecEnv {
    pub id: String,
    pub prepared: PreparedExec,
    pub executed: SandboxPolicy,
    pub args: Vec<String>,
}

pub(crate) fn prepare_batch_exec(
    mgr: &VmManager,
    cmd: &Command,
) -> Result<Vec<BatchExecEnv>, Response> {
    let ids = cmd.sandboxes.clone();
    if ids.is_empty() {
        return Err(Response::err("Missing 'sandboxes' field"));
    }
    if ids.len() > BATCH_MAX as usize {
        return Err(Response::err(format!("sandboxes must be <= {BATCH_MAX}")));
    }
    if cmd.args.is_empty() {
        return Err(Response::err("Missing 'args' field"));
    }
    let timeout = cmd.timeout_secs.unwrap_or(60).min(3600);
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let rec = mgr
            .sandbox_get(&id)
            .ok_or_else(|| Response::err(format!("sandbox '{}' not found", id)))?;
        let handle = mgr
            .sandbox_handle(&id)
            .ok_or_else(|| Response::err(format!("sandbox '{}' has no session handle", id)))?;
        let stored = rec.policy.clone();
        let executed = handle_executed_policy(stored.as_ref(), None);
        validate_exec_quota(mgr, &rec.vm_name, &executed)?;
        let cmd_args = sandbox_exec_command(&cmd.args, None, None, timeout);
        out.push(BatchExecEnv {
            id,
            prepared: PreparedExec::Sandbox {
                handle,
                cmd: cmd_args,
            },
            executed,
            args: cmd.args.clone(),
        });
    }
    Ok(out)
}

// ── batch_recycle ────────────────────────────────────────────────────────

pub(crate) enum BatchRecycleItem {
    Destroy(TenantDestroyPlan),
    Reset {
        tenant: String,
        handle: Arc<dyn VmHandle>,
    },
}

pub(crate) fn prepare_batch_recycle(
    mgr: &mut VmManager,
    cmd: &Command,
) -> Result<Vec<BatchRecycleItem>, Response> {
    let tenants = cmd.tenants.clone();
    if tenants.is_empty() {
        return Err(Response::err("Missing 'tenants' field"));
    }
    if tenants.len() > BATCH_MAX as usize {
        return Err(Response::err(format!("tenants must be <= {BATCH_MAX}")));
    }
    let mode = cmd.mode.clone().unwrap_or_else(|| "destroy".to_string());
    let mut out = Vec::with_capacity(tenants.len());
    for t in tenants {
        match mode.as_str() {
            "destroy" => {
                let mut tc = cmd.clone();
                tc.tenant = Some(t.clone());
                let plan = prepare_tenant_destroy(mgr, &tc)?;
                out.push(BatchRecycleItem::Destroy(plan));
            }
            "reset" => {
                let rec = mgr.sandbox_list(Some(&t)).into_iter().next();
                let vm_name = match rec {
                    Some(r) => r.vm_name.clone(),
                    None => format!("tenant-{}", t),
                };
                let handle = mgr
                    .get_handle(&vm_name)
                    .ok_or_else(|| Response::err(format!("VM '{}' not found", vm_name)))?;
                out.push(BatchRecycleItem::Reset {
                    tenant: t,
                    handle,
                });
            }
            other => {
                return Err(Response::err(format!(
                    "invalid mode {:?}: expected \"destroy\" or \"reset\"",
                    other
                )));
            }
        }
    }
    Ok(out)
}

fn env_vm_name(env: &BatchEnv) -> String {
    match &env.plan.vm {
        TenantVm::Existing { vm_name, .. } => vm_name.clone(),
        TenantVm::ColdSpec { vm_name, .. } => vm_name.clone(),
    }
}

/// Locked execute() path (tests + non-daemon callers). The slow parts are
/// still run in parallel via the shared helpers; the manager lock is held
/// for the whole command (acceptable outside the daemon).
pub(crate) async fn cmd_batch_create(mgr: &mut VmManager, cmd: Command) -> Response {
    let envs = match prepare_batch_create(mgr, &cmd).await {
        Ok(e) => e,
        Err(resp) => return resp,
    };
    let adapter = mgr.adapter();
    let sb_adapter = mgr.sandbox_adapter_arc();
    let spawned = spawn_batch_vms(&envs, adapter).await;

    // Register cold spawns + resolve every env's handle.
    let mut handles: HashMap<usize, Arc<dyn VmHandle>> = HashMap::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for (idx, handle) in spawned {
        if let TenantVm::ColdSpec { spec, .. } = &envs[idx].plan.vm {
            match mgr.register_vm(spec, handle) {
                Ok(()) => {
                    if let Some(h) = mgr.get_handle(&env_vm_name(&envs[idx])) {
                        handles.insert(idx, h);
                    }
                }
                Err(e) => failed.push((envs[idx].tenant.clone(), e.to_string())),
            }
        }
    }
    for (idx, env) in envs.iter().enumerate() {
        if let TenantVm::Existing { handle, .. } = &env.plan.vm {
            handles.insert(idx, handle.clone());
        }
    }

    let results = bind_batch_envs(&envs, &handles, sb_adapter).await;
    let mut created: Vec<serde_json::Value> = Vec::new();
    for (idx, r) in results {
        match r {
            Ok(record) => {
                finish_sandbox_create(mgr, &envs[idx].plan, record);
                created.push(serde_json::json!({
                    "tenant": envs[idx].tenant,
                    "id": envs[idx].plan.sandbox_id,
                    "vm": env_vm_name(&envs[idx]),
                }));
            }
            Err(e) => failed.push((envs[idx].tenant.clone(), e)),
        }
    }
    batch_create_response(created, failed)
}

pub(crate) fn batch_create_response(
    created: Vec<serde_json::Value>,
    failed: Vec<(String, String)>,
) -> Response {
    let failed_json: Vec<_> = failed
        .iter()
        .map(|(name, err)| serde_json::json!({"tenant": name, "error": err}))
        .collect();
    Response::ok(serde_json::json!({
        "envs": created,
        "count": created.len(),
        "failed": failed_json,
    }))
}

/// Run a batch of prepared execs in parallel (shared by locked + daemon
/// paths). Returns (sandbox id, response) pairs.
pub(crate) async fn run_batch_execs(
    envs: Vec<BatchExecEnv>,
) -> Vec<(String, Response)> {
    let mut set = tokio::task::JoinSet::new();
    for env in envs {
        set.spawn(async move {
            let resp = prepared_exec_audited(
                env.prepared,
                Some(&env.executed),
                &env.id,
                &env.args,
            )
            .await;
            (env.id, resp)
        });
    }
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(pair) = res {
            out.push(pair);
        }
    }
    out
}

pub(crate) async fn cmd_batch_exec(mgr: &mut VmManager, cmd: Command) -> Response {
    let envs = match prepare_batch_exec(mgr, &cmd) {
        Ok(e) => e,
        Err(resp) => return resp,
    };
    let results = run_batch_execs(envs).await;
    let rows: Vec<_> = results
        .into_iter()
        .map(|(id, resp)| {
            serde_json::json!({
                "id": id,
                "status": resp.status,
                "data": resp.data,
            })
        })
        .collect();
    Response::ok(serde_json::json!({"results": rows, "count": rows.len()}))
}

/// Run a batch of recycle items in parallel (destroy teardown or reset).
/// Returns per-tenant results; destroy finalization happens on re-lock.
pub(crate) async fn run_batch_recycle(
    items: &[BatchRecycleItem],
) -> Vec<(String, Result<bool, String>)> {
    let mut set = tokio::task::JoinSet::new();
    for item in items {
        match item {
            BatchRecycleItem::Destroy(plan) => {
                let plan = plan.clone();
                set.spawn(async move {
                    let tenant = plan.tenant.clone();
                    let r = finish_tenant_destroy(&plan).await;
                    (tenant, r)
                });
            }
            BatchRecycleItem::Reset {
                tenant,
                handle,
                ..
            } => {
                let tenant = tenant.clone();
                let handle = handle.clone();
                set.spawn(async move {
                    let r = handle.reset_fs().await.map_err(|e| e.to_string());
                    (tenant, r.map(|_| false))
                });
            }
        }
    }
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(pair) = res {
            out.push(pair);
        }
    }
    out
}

pub(crate) async fn cmd_batch_recycle(mgr: &mut VmManager, cmd: Command) -> Response {
    let items = match prepare_batch_recycle(mgr, &cmd) {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    let results = run_batch_recycle(&items).await;
    // Finalize destroy bookkeeping under the lock (reset keeps records).
    for item in &items {
        if let BatchRecycleItem::Destroy(plan) = item {
            finalize_tenant_destroy(mgr, plan);
        }
    }
    let rows: Vec<_> = results
        .into_iter()
        .map(|(tenant, r)| {
            let status = match &r {
                Ok(released) if *released => "released",
                Ok(_) => "reset",
                Err(_) => "error",
            };
            serde_json::json!({"tenant": tenant, "status": status, "error": r.err()})
        })
        .collect();
    Response::ok(serde_json::json!({"results": rows, "count": rows.len()}))
}

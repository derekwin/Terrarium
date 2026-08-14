//! Engine-level sandbox commands (S-M2).
//!
//! A sandbox is a workdir on a tenant's shared VM (`tenant-<tenant>`);
//! isolation inside the VM is enforced guest-side by sandlock. The engine
//! owns the registry: tenant → VM, sandbox id → {tenant, workdir}.

use std::sync::Arc;

use super::{apply_system_base, build_spec, run_exec, DEFAULT_SYSTEM};
use crate::manager::{SandboxRecord, VmManager};
use crate::policy::default_sandbox_policy;
use adapter_traits::{
    ExecOpts, ResourceLimits, SandboxAdapter, SandboxHandle, SandboxPolicy, SandboxSpec, VmHandle,
    VmName, VmSpec,
};
use terrarium_protocol::{Command, Response};

/// Fresh sandbox id: `sb-<12 hex>` (48 bits, uuid v4, no new dependency).
fn new_sandbox_id() -> String {
    format!("sb-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn record_json(r: &SandboxRecord) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "tenant": r.tenant,
        "vm": r.vm_name,
        "workdir": r.workdir,
        "created_at": r.created_at,
        "policy": r.policy,
        "pool_backed": r.pool_backed,
    })
}

/// The tenant's VM after resolution: an existing/claimed handle, or a
/// cold-boot spec the caller must spawn and register under
/// `tenant-<tenant>` (the daemon spawns it OUTSIDE the manager lock so
/// concurrent sandbox_create calls boot VMs in parallel).
#[derive(Clone)]
pub(crate) enum TenantVm {
    Existing {
        vm_name: String,
        pool_backed: bool,
        handle: Arc<dyn VmHandle>,
    },
    ColdSpec {
        vm_name: String,
        spec: VmSpec,
    },
}

/// Resolve the tenant's VM (cheap registry work only — no spawn):
/// - a sandbox record for this tenant already exists → reuse its VM
///   (pooled VMs are named pool-N, so indexing is by tenant, not name);
/// - else, if pooling is allowed and a matching idle slot exists → claim
///   it with the sandbox's layer set (ephemeral upper) and resize it when
///   the spec asks for cpus/memory;
/// - else return a cold-boot spec for `tenant-<tenant>` (same path as
///   `create`) — the caller decides whether to spawn under or outside the
///   manager lock.
async fn resolve_tenant_vm(
    mgr: &mut VmManager,
    cmd: &Command,
    tenant: &str,
) -> Result<TenantVm, Response> {
    if let Some(rec) = mgr.sandbox_list(Some(tenant)).into_iter().next() {
        let handle = mgr
            .get_handle(&rec.vm_name)
            .ok_or_else(|| Response::err(format!("VM '{}' not found", rec.vm_name)))?;
        return Ok(TenantVm::Existing {
            vm_name: rec.vm_name,
            pool_backed: rec.pool_backed,
            handle,
        });
    }

    // Pool path. A pooled VM needs a layered fs attached, so an empty
    // layer list means "just the system base". A claim failure (no pool,
    // no idle slot, net mismatch) falls through to the cold boot below.
    if cmd.pool.unwrap_or(true) {
        let mut pool_layers = cmd.layers.clone();
        if pool_layers.is_empty() {
            pool_layers.push(cmd.system.clone().unwrap_or_else(|| DEFAULT_SYSTEM.into()));
        }
        if let Ok(name) = mgr.pool_claim_matching(pool_layers, Some(cmd.net)).await {
            // Spec asks for cpus/memory differing from the pool boot
            // config (1 vCPU / 256 MB) → resize post-claim.  Dimensions
            // already at the requested size are skipped — CH rejects a
            // no-op resize ("new size ... identical").
            if cmd.cpus.is_some() || cmd.memory_mb.is_some() {
                let handle = match mgr.get_handle(&name) {
                    Some(h) => h,
                    None => return Err(Response::err(format!("VM '{}' not found", name))),
                };
                let current = match handle.info().await {
                    Ok(i) => i,
                    Err(e) => {
                        let _ = mgr.pool_release(&name).await;
                        return Err(Response::err(format!("pool VM info failed: {}", e)));
                    }
                };
                let want_cpus = cmd.cpus.map(|c| c as u32);
                let want_mem_bytes = cmd.memory_mb.map(|mb| mb * 1024 * 1024);
                // No CPU-shrink guard needed here: the requested cpus is
                // the boot spec (>= 1) and pool VMs boot at 1 vCPU, so a
                // claim only ever resizes up; identical dimensions are
                // filtered below as no-ops.
                let cpus = want_cpus.filter(|c| current.cpus != Some(*c as u8));
                let mem = want_mem_bytes.filter(|m| current.memory_mb != Some(*m / 1024 / 1024));
                if cpus.is_some() || mem.is_some() {
                    if let Err(e) = handle.resize(cpus, mem).await {
                        let _ = mgr.pool_release(&name).await;
                        return Err(Response::err(format!("pool VM resize failed: {}", e)));
                    }
                    // Sync the recorded policy with the post-claim
                    // allocation (the quota sandbox limits validate
                    // against, policy-model.md §3.5) — a stale boot-time
                    // entry (1 vCPU / 256 MB) would reject sandboxes the
                    // resized pool VM can host.
                    let memory_mb = mem.map(|b| b / 1024 / 1024);
                    if let Err(e) = mgr.record_resize(&name, cpus, memory_mb) {
                        let _ = mgr.pool_release(&name).await;
                        return Err(Response::err(format!("pool VM policy sync failed: {}", e)));
                    }
                }
            }
            let handle = match mgr.get_handle(&name) {
                Some(h) => h,
                None => return Err(Response::err(format!("VM '{}' not found", name))),
            };
            return Ok(TenantVm::Existing {
                vm_name: name,
                pool_backed: true,
                handle,
            });
        }
    }

    let vm_name = format!("tenant-{}", tenant);
    if mgr.get(&vm_name).is_none() {
        let mut cmd = cmd.clone();
        apply_system_base(&mut cmd);
        cmd.name = Some(vm_name.clone());
        let spec = match build_spec(&cmd) {
            Ok(s) => s,
            Err(e) => return Err(Response::err(e)),
        };
        if let Err(e) = spec.validate() {
            return Err(Response::err(e));
        }
        return Ok(TenantVm::ColdSpec { vm_name, spec });
    }
    let handle = mgr
        .get_handle(&vm_name)
        .ok_or_else(|| Response::err(format!("VM '{}' not found", vm_name)))?;
    Ok(TenantVm::Existing {
        vm_name,
        pool_backed: false,
        handle,
    })
}

/// Everything `sandbox_create` needs that can be resolved under the
/// manager lock, so the daemon can run the slow parts (VM boot, agent
/// ready, workdir) outside it.
#[derive(Clone)]
pub(crate) struct PreparedSandboxCreate {
    pub tenant: String,
    pub vm: TenantVm,
    pub sandbox_id: String,
    pub workdir: String,
    pub effective: SandboxPolicy,
    pub user: Option<SandboxPolicy>,
}

/// Validate + resolve the tenant VM + allocate the sandbox id — all cheap,
/// lock-appropriate work. The caller performs the actual spawn/bind.
pub(crate) async fn prepare_sandbox_create(
    mgr: &mut VmManager,
    cmd: &Command,
) -> Result<PreparedSandboxCreate, Response> {
    let tenant = cmd
        .tenant
        .clone()
        .ok_or_else(|| Response::err("Missing 'tenant' field"))?;
    // Same whitelist as VmName — the tenant may be embedded in a VM name.
    if let Err(e) = VmName::new(tenant.clone()) {
        return Err(Response::err(format!("invalid tenant: {}", e)));
    }
    // Captured before `cmd` is consumed by the VM-create branch below.
    let policy = cmd.policy.clone();
    // Fail fast: an invalid stored policy would fail on every later exec.
    if let Some(p) = policy.as_ref() {
        if let Err(err) = p.validate() {
            return Err(Response::err(err));
        }
    }

    let vm = resolve_tenant_vm(mgr, cmd, &tenant).await?;

    // G2: the two-layer invariant — sandbox limits ⊆ VM quota
    // (policy-model.md §3.5). For an existing VM the policy is registered;
    // for a cold boot it is exactly `spec.to_policy()` (what register_vm
    // stores). No limits → trivially valid.
    if let Some(user) = policy.as_ref() {
        let quota_err = match &vm {
            TenantVm::Existing { vm_name, .. } => mgr
                .vm_policy(vm_name)
                .map(|p| user.validate_with_vm(p)),
            TenantVm::ColdSpec { spec, .. } => {
                let p = spec.to_policy();
                Some(user.validate_with_vm(&p))
            }
        };
        if let Some(Err(err)) = quota_err {
            return Err(Response::err(err));
        }
    }

    // 48-bit suffix: a collision with a live record would silently drop
    // its workdir mapping (bare HashMap::insert), so re-roll until free.
    let mut id = new_sandbox_id();
    while mgr.sandbox_get(&id).is_some() {
        id = new_sandbox_id();
    }
    // The effective policy (engine default ∪ user) is fixed at create and
    // carried by the returned handle; later per-call overrides union onto
    // it (never a replace).
    let effective = match policy.clone() {
        Some(user) => default_sandbox_policy().merged_with(&user),
        None => default_sandbox_policy(),
    };
    let workdir = format!("/workdir/{}", id);
    Ok(PreparedSandboxCreate {
        tenant,
        vm,
        sandbox_id: id,
        workdir,
        effective,
        user: policy,
    })
}

/// Bind the L2 session through the SandboxAdapter and ensure the workdir
/// exists (slow: includes agent-boot vsock retries). Returns the sandbox
/// record; the caller registers it under the manager lock.
pub(crate) async fn bind_sandbox(
    sb_adapter: &dyn SandboxAdapter,
    handle: Arc<dyn VmHandle>,
    prepared: &PreparedSandboxCreate,
) -> Result<SandboxRecord, Response> {
    let spec_name = match VmName::new(prepared.sandbox_id.clone()) {
        Ok(n) => n,
        Err(e) => return Err(Response::err(format!("invalid sandbox name: {}", e))),
    };
    let spec = SandboxSpec {
        name: spec_name,
        limits: ResourceLimits::default(),
        policy: Some(prepared.effective.clone()),
    };
    let sb_handle = match sb_adapter.create(handle.clone(), &spec).await {
        Ok(h) => h,
        Err(e) => return Err(Response::err(e.to_string())),
    };

    // Ensure the workdir exists in the guest (unsandboxed). On failure
    // return an honest error, best-effort tear down the bound session, and
    // don't register a half-created sandbox.
    let mkdir = vec!["mkdir".to_string(), "-p".to_string(), prepared.workdir.clone()];
    let opts = ExecOpts::new(mkdir, 30).with_sandbox(false);
    // The guest agent can still be booting right after CH reports ready —
    // a slow layer (e.g. ubuntu) often needs a moment before the vsock
    // listener is up. Retry the exec briefly on handshake/vsock failures
    // (same pattern as attach_fs's agent-boot retry) so creation does not
    // fail on a race.
    let mut workdir_result = handle.exec(&opts).await;
    for _ in 0..10 {
        let retryable = matches!(&workdir_result, Err(e)
            if e.to_string().contains("vsock") || e.to_string().contains("handshake"));
        if !retryable {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        workdir_result = handle.exec(&opts).await;
    }
    let (vm_name, pool_backed) = match &prepared.vm {
        TenantVm::Existing {
            vm_name,
            pool_backed,
            ..
        } => (vm_name.clone(), *pool_backed),
        TenantVm::ColdSpec { vm_name, .. } => (vm_name.clone(), false),
    };
    match workdir_result {
        Ok(r) if r.exit_code == 0 => {}
        Ok(r) => {
            let _ = sb_handle.destroy().await;
            return Err(Response::err(format!(
                "failed to create workdir {}: {}",
                prepared.workdir,
                r.stderr.trim()
            )));
        }
        Err(e) => {
            let _ = sb_handle.destroy().await;
            return Err(Response::err(format!(
                "failed to create workdir {}: {}",
                prepared.workdir, e
            )));
        }
    }
    Ok(SandboxRecord {
        id: prepared.sandbox_id.clone(),
        tenant: prepared.tenant.clone(),
        vm_name,
        workdir: prepared.workdir.clone(),
        created_at: now_secs(),
        policy: prepared.user.clone(),
        pool_backed,
        handle: Some(Arc::from(sb_handle)),
    })
}

/// Register the sandbox record + D-phase limits audit + build the
/// response. Shared by the locked `execute` path and the daemon's
/// lock-free fast path.
pub(crate) fn finish_sandbox_create(
    mgr: &mut VmManager,
    prepared: &PreparedSandboxCreate,
    record: SandboxRecord,
) -> Response {
    // D-phase audit: the declared resource limits are a resource
    // declaration, recorded when the (stored user) policy asks for it.
    // Limits only ever come from the user layer — the engine default
    // carries none — so the stored user policy both carries and gates them.
    if let Some(user) = prepared.user.as_ref() {
        crate::audit::audit_resource(
            Some(user),
            &prepared.sandbox_id,
            "limits",
            &format!("{:?}", user.limits),
        );
    }
    let (vm_name, pool_backed) = match &prepared.vm {
        TenantVm::Existing {
            vm_name,
            pool_backed,
            ..
        } => (vm_name.clone(), *pool_backed),
        TenantVm::ColdSpec { vm_name, .. } => (vm_name.clone(), false),
    };
    mgr.sandbox_insert(record);
    Response::ok(serde_json::json!({
        "id": prepared.sandbox_id,
        "vm": vm_name,
        "workdir": prepared.workdir,
        "pool": pool_backed,
    }))
}

/// {"command":"sandbox_create","tenant":"research", kernel/layers/...,
///  "pool":bool}
///
/// Idempotently ensures the tenant VM exists (from the warm pool when
/// possible, else cold-booted; VM-spec fields mirror `create`), then
/// allocates a sandbox and creates its workdir in the guest.
/// Response data: {id, vm, workdir, pool}.
pub(crate) async fn cmd_sandbox_create(mgr: &mut VmManager, cmd: Command) -> Response {
    // Locked execute() path (tests + non-daemon callers): spawn the cold
    // VM under the lock, same semantics as before the split.
    let prepared = match prepare_sandbox_create(mgr, &cmd).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let handle = match &prepared.vm {
        TenantVm::Existing { handle, .. } => handle.clone(),
        TenantVm::ColdSpec { vm_name, spec } => {
            if let Err(e) = mgr.spawn(spec.clone()).await {
                return Response::err(e.to_string());
            }
            match mgr.get_handle(vm_name) {
                Some(h) => h,
                None => return Response::err(format!("VM '{}' not found", vm_name)),
            }
        }
    };
    let record = match bind_sandbox(mgr.sandbox_adapter(), handle, &prepared).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    finish_sandbox_create(mgr, &prepared, record)
}

/// {"command":"sandbox_exec","id":"sb-...","args":[...],"timeout_secs":N,
///  "sandbox":bool,"exec_mode":...,"policy":{...}}
///
/// Runs in the tenant VM with cwd = the sandbox workdir. Confinement
/// defaults to ON (absent `sandbox` → true). A per-call `policy` overrides
/// the policy stored at sandbox_create; a policy with `sandbox:false` is
/// rejected.
pub(crate) async fn cmd_sandbox_exec(mgr: &mut VmManager, cmd: Command) -> Response {
    let id = match cmd.id.clone() {
        Some(i) => i,
        None => return Response::err("Missing 'id' field"),
    };
    let record = match mgr.sandbox_get(&id) {
        Some(r) => r,
        None => return Response::err(format!("Sandbox '{}' not found", id)),
    };
    // D2: sandboxed exec always carries a complete policy — the session
    // handle bound at create already holds the effective (default ∪ stored)
    // policy. `run_exec` passes only the per-call override; it injects the
    // engine default for direct paths when no policy resolves. An explicit
    // `sandbox:false` escape hatch stays policy-free.
    run_exec(
        mgr,
        &record.vm_name,
        &cmd.args,
        cmd.timeout_secs,
        true, // sandboxed by default.
        cmd.sandbox,
        cmd.policy.clone(),
        Some(&record.workdir),
        cmd.exec_mode.clone(),
        Some(id.clone()),
    )
    .await
}

/// {"command":"sandbox_list","tenant"?}
pub(crate) fn cmd_sandbox_list(mgr: &VmManager, cmd: Command) -> Response {
    let records = mgr.sandbox_list(cmd.tenant.as_deref());
    let items: Vec<_> = records.iter().map(record_json).collect();
    Response::ok(serde_json::json!({
        "sandboxes": items,
        "count": items.len(),
    }))
}

/// {"command":"sandbox_info","id":"sb-..."}
pub(crate) fn cmd_sandbox_info(mgr: &VmManager, cmd: Command) -> Response {
    let id = match cmd.id {
        Some(i) => i,
        None => return Response::err("Missing 'id' field"),
    };
    match mgr.sandbox_get(&id) {
        Some(r) => Response::ok(record_json(&r)),
        None => Response::err(format!("Sandbox '{}' not found", id)),
    }
}

/// {"command":"sandbox_kill","id":"sb-..."}
///
/// Kills every live session of this sandbox, removes the workdir from the
/// guest, and drops the record. The shared tenant VM keeps running.
pub(crate) async fn cmd_sandbox_kill(mgr: &mut VmManager, cmd: Command) -> Response {
    let id = match cmd.id.clone() {
        Some(i) => i,
        None => return Response::err("Missing 'id' field"),
    };
    let record = match mgr.sandbox_get(&id) {
        Some(r) => r,
        None => return Response::err(format!("Sandbox '{}' not found", id)),
    };

    // Kill every live session registered for this sandbox.
    let live: Vec<_> = mgr
        .session_list()
        .into_iter()
        .filter(|s| s.sandbox.as_deref() == Some(id.as_str()) && s.status == "running")
        .collect();
    for s in &live {
        if let Err(e) = mgr.session_kill(&s.session_id).await {
            return Response::err(format!("failed to kill session '{}': {}", s.session_id, e));
        }
    }

    // Untrusted-input discipline: only ever rm a path we created.
    if !record.workdir.starts_with("/workdir/sb-") {
        return Response::err(format!(
            "refusing to remove unexpected workdir path {:?}",
            record.workdir
        ));
    }
    let rm = vec!["rm".to_string(), "-rf".to_string(), record.workdir.clone()];
    match mgr.exec(&record.vm_name, &rm, 30, false, None, None).await {
        Ok(r) if r.exit_code == 0 => {}
        Ok(r) => {
            return Response::err(format!(
                "failed to remove workdir {}: {}",
                record.workdir,
                r.stderr.trim()
            ));
        }
        Err(e) => {
            return Response::err(format!(
                "failed to remove workdir {}: {}",
                record.workdir, e
            ));
        }
    }

    // C3: best-effort teardown through the bound session handle (the VM
    // teardown is the real cleanup; GuestSandlockHandle::destroy is a no-op).
    if let Some(handle) = &record.handle {
        if let Err(e) = handle.destroy().await {
            tracing::warn!(sandbox = %id, error = %e, "handle.destroy failed during sandbox_kill");
        }
    }

    mgr.sandbox_remove(&id);
    Response::ok(serde_json::json!({
        "id": id,
        "sessions_killed": live.len(),
        "status": "killed",
    }))
}

/// {"command":"tenant_destroy","tenant":"research"}
///
/// Tears the tenant down: kills live sessions of its sandboxes, then —
/// for a pool-backed tenant VM — releases the VM back to the pool (fs
/// detached, slot idle again), else destroys the VM (same semantics as
/// `destroy`). All of the tenant's sandbox records are dropped either way.
/// Everything `tenant_destroy` needs resolved under the manager lock; the
/// caller runs the vsock/CH work outside it and finalizes on re-lock.
#[derive(Clone)]
pub(crate) struct TenantDestroyPlan {
    pub tenant: String,
    pub vm_name: String,
    pub pool_backed: bool,
    pub pool_ready: bool,
    pub session_ids: Vec<String>,
    pub sandbox_handles: Vec<Arc<dyn SandboxHandle>>,
    pub vm_handle: Arc<dyn VmHandle>,
    pub removed: usize,
}

/// Resolve the tenant teardown plan (cheap registry work only).
pub(crate) fn prepare_tenant_destroy(
    mgr: &mut VmManager,
    cmd: &Command,
) -> Result<TenantDestroyPlan, Response> {
    let tenant = cmd
        .tenant
        .clone()
        .ok_or_else(|| Response::err("Missing 'tenant' field"))?;
    if let Err(e) = VmName::new(tenant.clone()) {
        return Err(Response::err(format!("invalid tenant: {}", e)));
    }

    // Resolve the tenant VM from the registry (pooled VMs are pool-N);
    // fall back to the cold-boot name convention when no records exist.
    let records = mgr.sandbox_list(Some(&tenant));
    let (vm_name, pool_backed) = match records.first() {
        Some(r) => (r.vm_name.clone(), r.pool_backed),
        None => (format!("tenant-{}", tenant), false),
    };
    if mgr.get(&vm_name).is_none() {
        return Err(Response::err(format!("VM '{}' not found", vm_name)));
    }

    let sb_ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
    let session_ids: Vec<String> = mgr
        .session_list()
        .into_iter()
        .filter(|s| {
            s.status == "running"
                && s.sandbox
                    .as_deref()
                    .is_some_and(|id| sb_ids.iter().any(|x| x == id))
        })
        .map(|s| s.session_id)
        .collect();
    let sandbox_handles: Vec<Arc<dyn SandboxHandle>> =
        records.iter().filter_map(|r| r.handle.clone()).collect();
    let vm_handle = mgr
        .get_handle(&vm_name)
        .ok_or_else(|| Response::err(format!("VM '{}' not found", vm_name)))?;
    let pool_ready = pool_backed && mgr.pool_slot_ready(&vm_name);
    let removed = records.len();

    // Cold (non-pool) VMs: unregister under the lock; the handle's
    // shutdown runs lock-free (same as the daemon's destroy path).
    if !pool_backed {
        mgr.unregister(&vm_name);
    }
    Ok(TenantDestroyPlan {
        tenant,
        vm_name,
        pool_backed,
        pool_ready,
        session_ids,
        sandbox_handles,
        vm_handle,
        removed,
    })
}

/// Run the slow teardown WITHOUT the manager lock: guest session kills,
/// sandbox session destroys, then the VM reset/detach/shutdown. Returns
/// `true` when the VM was released to the pool.
pub(crate) async fn finish_tenant_destroy(
    plan: &TenantDestroyPlan,
) -> Result<bool, String> {
    // Kill live sessions of this tenant's sandboxes (best effort: the VM
    // teardown below is the real cleanup, so log and continue).
    for sid in &plan.session_ids {
        if let Err(e) = plan.vm_handle.kill_exec(sid).await {
            tracing::warn!(session = %sid, error = %e, "session kill failed during tenant_destroy");
        }
    }
    // C3: best-effort teardown through each bound session handle.
    for h in &plan.sandbox_handles {
        if let Err(e) = h.destroy().await {
            tracing::warn!(error = %e, "sandbox handle.destroy failed during tenant_destroy");
        }
    }
    if plan.pool_backed {
        if plan.pool_ready {
            // In-place reset back to the LAYER baseline (ready state lives
            // in the layer; episode writes land in the ephemeral upper).
            plan.vm_handle.reset_fs().await.map_err(|e| e.to_string())?;
        } else {
            plan.vm_handle.detach_fs().await.map_err(|e| e.to_string())?;
        }
        Ok(true)
    } else {
        plan.vm_handle.shutdown().await.map_err(|e| e.to_string())?;
        Ok(false)
    }
}

/// Shared finalize: session statuses, pool slot bookkeeping, tenant records.
pub(crate) fn finalize_tenant_destroy(mgr: &mut VmManager, plan: &TenantDestroyPlan) {
    mgr.sessions_mark_killed(&plan.session_ids);
    if plan.pool_backed {
        mgr.pool_mark_released(&plan.vm_name, plan.pool_ready);
    }
    mgr.sandbox_remove_tenant(&plan.tenant);
}

pub(crate) async fn cmd_tenant_destroy(mgr: &mut VmManager, cmd: Command) -> Response {
    let plan = match prepare_tenant_destroy(mgr, &cmd) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let released_to_pool = match finish_tenant_destroy(&plan).await {
        Ok(r) => r,
        Err(e) => return Response::err(e),
    };
    finalize_tenant_destroy(mgr, &plan);
    Response::ok(serde_json::json!({
        "tenant": plan.tenant,
        "vm": plan.vm_name,
        "sandboxes_removed": plan.removed,
        "released_to_pool": released_to_pool,
        "status": if released_to_pool { "released" } else { "destroyed" },
    }))
}

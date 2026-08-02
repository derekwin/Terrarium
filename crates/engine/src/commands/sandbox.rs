//! Engine-level sandbox commands (S-M2).
//!
//! A sandbox is a workdir on a tenant's shared VM (`tenant-<tenant>`);
//! isolation inside the VM is enforced guest-side by sandlock. The engine
//! owns the registry: tenant → VM, sandbox id → {tenant, workdir}.

use std::sync::Arc;

use super::{apply_system_base, build_spec, run_exec, DEFAULT_SYSTEM};
use crate::manager::{SandboxRecord, VmManager};
use crate::policy::default_sandbox_policy;
use adapter_traits::{ResourceLimits, SandboxSpec, VmName};
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

/// Resolve the tenant's VM, creating it if needed:
/// - a sandbox record for this tenant already exists → reuse its VM
///   (pooled VMs are named pool-N, so indexing is by tenant, not name);
/// - else, if pooling is allowed and a matching idle slot exists → claim
///   it with the sandbox's layer set (ephemeral upper) and resize it when
///   the spec asks for cpus/memory;
/// - else cold-boot `tenant-<tenant>` via the same path as `create`.
///
/// Returns (vm_name, pool_backed).
async fn ensure_tenant_vm(
    mgr: &mut VmManager,
    cmd: &Command,
    tenant: &str,
) -> Result<(String, bool), Response> {
    if let Some(rec) = mgr.sandbox_list(Some(tenant)).into_iter().next() {
        return Ok((rec.vm_name, rec.pool_backed));
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
            return Ok((name, true));
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
        if let Err(e) = mgr.spawn(spec).await {
            return Err(Response::err(e.to_string()));
        }
    }
    Ok((vm_name, false))
}

/// {"command":"sandbox_create","tenant":"research", kernel/layers/...,
///  "pool":bool}
///
/// Idempotently ensures the tenant VM exists (from the warm pool when
/// possible, else cold-booted; VM-spec fields mirror `create`), then
/// allocates a sandbox and creates its workdir in the guest.
/// Response data: {id, vm, workdir, pool}.
pub(crate) async fn cmd_sandbox_create(mgr: &mut VmManager, cmd: Command) -> Response {
    let tenant = match cmd.tenant.clone() {
        Some(t) => t,
        None => return Response::err("Missing 'tenant' field"),
    };
    // Same whitelist as VmName — the tenant may be embedded in a VM name.
    if let Err(e) = VmName::new(tenant.clone()) {
        return Response::err(format!("invalid tenant: {}", e));
    }
    // Captured before `cmd` is consumed by the VM-create branch below.
    let policy = cmd.policy.clone();
    // Fail fast: an invalid stored policy would fail on every later exec.
    if let Some(p) = policy.as_ref() {
        if let Err(err) = p.validate() {
            return Response::err(err);
        }
    }

    let (vm_name, pool_backed) = match ensure_tenant_vm(mgr, &cmd, &tenant).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    // G2: the two-layer invariant — sandbox limits ⊆ VM quota
    // (policy-model.md §3.5). The tenant VM is registered by now (spawn
    // stored its `VmPolicy`), so validate the user's requested limits
    // against the VM's physical quota and reject the create on violation.
    // No limits → trivially valid.
    if let Some(user) = policy.as_ref() {
        if let Some(vm_policy) = mgr.vm_policy(&vm_name) {
            if let Err(err) = user.validate_with_vm(vm_policy) {
                return Response::err(err);
            }
        }
    }

    // 48-bit suffix: a collision with a live record would silently drop
    // its workdir mapping (bare HashMap::insert), so re-roll until free.
    let mut id = new_sandbox_id();
    while mgr.sandbox_get(&id).is_some() {
        id = new_sandbox_id();
    }
    let workdir = format!("/workdir/{}", id);

    // C3: bind the L2 session through the SandboxAdapter before registering
    // the record. The effective policy (engine default ∪ user) is fixed at
    // create and carried by the returned handle; later per-call overrides
    // union onto it (never a replace).
    let spec_name = match VmName::new(id.clone()) {
        Ok(n) => n,
        Err(e) => return Response::err(format!("invalid sandbox name: {}", e)),
    };
    let effective = match policy.clone() {
        Some(user) => default_sandbox_policy().merged_with(&user),
        None => default_sandbox_policy(),
    };
    let spec = SandboxSpec {
        name: spec_name,
        tools: Vec::new(),
        limits: ResourceLimits::default(),
        env: Default::default(),
        policy: Some(effective),
    };
    let vm_arc = match mgr.get_handle(&vm_name) {
        Some(h) => h,
        None => return Response::err(format!("VM '{}' not found", vm_name)),
    };
    let handle = match mgr.sandbox_adapter().create(vm_arc, &spec).await {
        Ok(h) => h,
        Err(e) => return Response::err(e.to_string()),
    };

    // Ensure the workdir exists in the guest (unsandboxed). On failure
    // return an honest error, best-effort tear down the bound session, and
    // don't register a half-created sandbox.
    let mkdir = vec!["mkdir".to_string(), "-p".to_string(), workdir.clone()];
    match mgr.exec(&vm_name, &mkdir, 30, false, None, None).await {
        Ok(r) if r.exit_code == 0 => {}
        Ok(r) => {
            let _ = handle.destroy().await;
            return Response::err(format!(
                "failed to create workdir {}: {}",
                workdir,
                r.stderr.trim()
            ));
        }
        Err(e) => {
            let _ = handle.destroy().await;
            return Response::err(format!("failed to create workdir {}: {}", workdir, e));
        }
    }

    let record = SandboxRecord {
        id: id.clone(),
        tenant,
        vm_name: vm_name.clone(),
        workdir: workdir.clone(),
        created_at: now_secs(),
        policy: policy.clone(),
        pool_backed,
        handle: Some(Arc::from(handle)),
    };
    mgr.sandbox_insert(record);
    // D-phase audit: the declared resource limits are a resource
    // declaration, recorded when the (stored user) policy asks for it.
    // Limits only ever come from the user layer — the engine default
    // carries none — so the stored user policy both carries and gates them.
    if let Some(user) = policy.as_ref() {
        crate::audit::audit_resource(Some(user), &id, "limits", &format!("{:?}", user.limits));
    }
    Response::ok(serde_json::json!({
        "id": id,
        "vm": vm_name,
        "workdir": workdir,
        "pool": pool_backed,
    }))
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
pub(crate) async fn cmd_tenant_destroy(mgr: &mut VmManager, cmd: Command) -> Response {
    let tenant = match cmd.tenant {
        Some(t) => t,
        None => return Response::err("Missing 'tenant' field"),
    };
    if let Err(e) = VmName::new(tenant.clone()) {
        return Response::err(format!("invalid tenant: {}", e));
    }

    // Resolve the tenant VM from the registry (pooled VMs are pool-N);
    // fall back to the cold-boot name convention when no records exist.
    let records = mgr.sandbox_list(Some(&tenant));
    let (vm_name, pool_backed) = match records.first() {
        Some(r) => (r.vm_name.clone(), r.pool_backed),
        None => (format!("tenant-{}", tenant), false),
    };
    if mgr.get(&vm_name).is_none() {
        return Response::err(format!("VM '{}' not found", vm_name));
    }

    // Kill live sessions of this tenant's sandboxes (best effort: the VM
    // teardown below is the real cleanup, so log and continue).
    let sb_ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    let live: Vec<_> = mgr
        .session_list()
        .into_iter()
        .filter(|s| {
            s.status == "running" && s.sandbox.as_deref().is_some_and(|id| sb_ids.contains(&id))
        })
        .collect();
    for s in &live {
        if let Err(e) = mgr.session_kill(&s.session_id).await {
            tracing::warn!(session = %s.session_id, error = %e, "session kill failed during tenant_destroy");
        }
    }

    // C3: best-effort teardown through each bound session handle (the VM
    // teardown/release below is the real cleanup).
    for rec in &records {
        if let Some(handle) = &rec.handle {
            if let Err(e) = handle.destroy().await {
                tracing::warn!(sandbox = %rec.id, error = %e, "handle.destroy failed during tenant_destroy");
            }
        }
    }

    // Captured up front: a cold `destroy` already cascades the records
    // (vm_name match), so counting afterwards would read 0.
    let removed = records.len();
    let released_to_pool = if pool_backed {
        match mgr.pool_release(&vm_name).await {
            Ok(()) => true,
            Err(e) => return Response::err(e.to_string()),
        }
    } else {
        match mgr.destroy(&vm_name).await {
            Ok(()) => false,
            Err(e) => return Response::err(e.to_string()),
        }
    };
    mgr.sandbox_remove_tenant(&tenant);
    Response::ok(serde_json::json!({
        "tenant": tenant,
        "vm": vm_name,
        "sandboxes_removed": removed,
        "released_to_pool": released_to_pool,
        "status": if released_to_pool { "released" } else { "destroyed" },
    }))
}

//! Tests for the resize → recorded `VmPolicy` sync.
//!
//! `VmManager::vm_policy` is the quota sandbox resource limits are
//! validated against (`SandboxPolicy::validate_with_vm`, policy-model.md
//! §3.5). It is recorded at spawn, so every successful resize — the
//! `resize` command and the pool-claim post-claim resize — must update it,
//! or the G2 check validates against the stale boot-time quota: a grown VM
//! rejects sandboxes it can now host (false negative), a shrunk VM admits
//! sandboxes beyond its physical allocation (over-admission).

mod common;

use std::sync::Arc;

use adapter_traits::{DefaultAccess, ResourceLimits, SandboxPolicy};
use common::MockVmAdapter;
use terrarium_engine::commands::execute;
use terrarium_engine::manager::VmManager;
use terrarium_protocol::Command;

fn make_mgr() -> VmManager {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    VmManager::new(Arc::new(adapter), "/tmp".into())
}

/// Build a default-deny capability policy carrying the given limits.
fn make_policy(limits: ResourceLimits) -> SandboxPolicy {
    SandboxPolicy {
        capabilities: vec![],
        limits,
        default: DefaultAccess::Deny,
        audit: Default::default(),
        version: 1,
    }
}

/// The `resize` command must sync the recorded policy's current
/// allocation with the new dimensions — both directions (memory grow and
/// shrink via virtio-mem) — while leaving the `max_*` ceilings untouched.
#[tokio::test]
async fn test_resize_updates_recorded_policy() {
    let mut mgr = make_mgr();
    // Boot at 1 vCPU / 256 MB with a 1024 MB ceiling.
    execute(
        &mut mgr,
        Command::create("rsz-vm", "/fake/vmlinux")
            .with_cpus(1)
            .with_memory_mb(256)
            .with_max_memory_mb(1024),
    )
    .await;
    assert_eq!(
        mgr.vm_policy("rsz-vm").unwrap().resources.memory_mb,
        256,
        "boot-time quota is recorded at spawn"
    );
    assert_eq!(mgr.vm_policy("rsz-vm").unwrap().resources.cpus, 1);

    // Grow to 4 vCPUs / 1024 MB.
    let resp = execute(
        &mut mgr,
        Command::new("resize")
            .with_name("rsz-vm")
            .with_cpus(4)
            .with_memory_bytes(1024 * 1024 * 1024),
    )
    .await;
    assert!(resp.is_ok(), "grow should succeed: {:?}", resp);

    let policy = mgr.vm_policy("rsz-vm").unwrap();
    assert_eq!(
        policy.resources.memory_mb, 1024,
        "recorded quota must follow the resize (not the boot-time 256)"
    );
    assert_eq!(
        policy.resources.cpus, 4,
        "recorded cpus must follow the resize"
    );
    assert_eq!(
        policy.resources.max_memory_mb,
        Some(1024),
        "max ceiling is a limit, not the current allocation"
    );
    assert_eq!(
        policy.resources.max_cpus, None,
        "max ceiling is a limit, not the current allocation"
    );

    // Shrink back to 256 MB (memory shrink is supported; virtio-mem).
    let resp = execute(
        &mut mgr,
        Command::new("resize")
            .with_name("rsz-vm")
            .with_memory_bytes(256 * 1024 * 1024),
    )
    .await;
    assert!(resp.is_ok(), "shrink should succeed: {:?}", resp);
    assert_eq!(
        mgr.vm_policy("rsz-vm").unwrap().resources.memory_mb,
        256,
        "shrink must sync the quota down too (over-admission guard)"
    );
}

/// G2 end-to-end (policy-model.md §3.5): a sandbox whose memory limit fits
/// the CURRENT quota of a resized tenant VM must be accepted. Against the
/// stale boot-time quota (256 MB) the same 512 MB limit was rejected —
/// the false negative this sync fixes.
#[tokio::test]
async fn test_resize_then_sandbox_limits_validate_against_current_quota() {
    let mut mgr = make_mgr();
    // Cold-boot the tenant VM at 256 MB.
    let first = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_memory_mb(256),
    )
    .await;
    assert!(first.is_ok(), "first sandbox_create: {:?}", first);
    assert_eq!(first.data.unwrap()["vm"], "tenant-research");
    assert_eq!(
        mgr.vm_policy("tenant-research")
            .unwrap()
            .resources
            .memory_mb,
        256
    );

    // Grow the tenant VM to 1024 MB.
    let resp = execute(
        &mut mgr,
        Command::new("resize")
            .with_name("tenant-research")
            .with_memory_bytes(1024 * 1024 * 1024),
    )
    .await;
    assert!(resp.is_ok(), "resize: {:?}", resp);
    assert_eq!(
        mgr.vm_policy("tenant-research")
            .unwrap()
            .resources
            .memory_mb,
        1024,
        "recorded quota must follow the resize"
    );

    // 512 MB limit ⊆ 1024 MB current quota → accepted (was rejected
    // against the stale 256 MB boot quota).
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_policy(make_policy(ResourceLimits {
                memory_mb: Some(512),
                ..Default::default()
            })),
    )
    .await;
    assert!(
        resp.is_ok(),
        "512 MB limit on a VM resized to 1024 MB must be accepted: {:?}",
        resp
    );
}

/// Pool claim with cpus/memory asks resizes the pool VM post-claim; the
/// recorded policy must follow so the G2 check on the claimed VM sees the
/// current allocation (pool VMs boot at 1 vCPU / 256 MB).
#[tokio::test]
async fn test_pool_claim_resize_updates_recorded_policy() {
    let mut mgr = make_mgr();
    let resp = execute(&mut mgr, Command::new("pool_create").with_pool_size(1)).await;
    assert!(resp.is_ok(), "pool_create: {:?}", resp);
    assert_eq!(
        mgr.vm_policy("pool-0").unwrap().resources.memory_mb,
        256,
        "pool VMs boot at 256 MB"
    );

    // Claim with 2 vCPUs / 1024 MB and a 512 MB sandbox limit: the
    // post-claim resize must raise the recorded quota before the G2 check.
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_cpus(2)
            .with_memory_mb(1024)
            .with_policy(make_policy(ResourceLimits {
                memory_mb: Some(512),
                ..Default::default()
            })),
    )
    .await;
    assert!(resp.is_ok(), "pool claim with resize: {:?}", resp);
    assert_eq!(resp.data.unwrap()["vm"], "pool-0");

    let policy = mgr.vm_policy("pool-0").unwrap();
    assert_eq!(
        policy.resources.memory_mb, 1024,
        "recorded quota must follow the post-claim resize"
    );
    assert_eq!(
        policy.resources.cpus, 2,
        "recorded cpus must follow the post-claim resize"
    );
}

/// `record_resize` on a VM that is not registered → explicit error, no
/// panic, no silent success: the caller must never believe the quota
/// synced when it did not.
#[tokio::test]
async fn test_record_resize_unregistered_vm_errors() {
    let mut mgr = make_mgr();
    let err = mgr
        .record_resize("ghost-vm", Some(4), Some(1024))
        .unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "explicit not-found error expected: {}",
        err
    );
}

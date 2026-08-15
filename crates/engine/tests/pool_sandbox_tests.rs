//! Tests for warm-pool integration with engine-level sandboxes:
//! pool-backed tenant VMs, cold fallback, tenant_destroy release,
//! destroy cascade, and pool_create readiness probing.

mod common;

use std::sync::Arc;

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

fn sandbox_create_cmd(tenant: &str) -> Command {
    Command::create("unused", "/fake/vmlinux")
        .with_command("sandbox_create")
        .with_tenant(tenant)
}

/// Boot `n` idle pool VMs through the command layer.
async fn create_pool(mgr: &mut VmManager, n: u32) {
    let resp = execute(mgr, Command::new("pool_create").with_pool_size(n)).await;
    assert!(resp.is_ok(), "pool_create should succeed: {:?}", resp);
}

/// sandbox_create claims an idle pool slot: vm_name is pool-N, the
/// response says pool:true, the slot is claimed with the sandbox's layers
/// (system base when none given), and a second sandbox of the same tenant
/// reuses the same pool-backed VM.
#[tokio::test]
async fn test_sandbox_create_claims_pool_slot() {
    let mut mgr = make_mgr();
    create_pool(&mut mgr, 1).await;

    let resp = execute(&mut mgr, sandbox_create_cmd("research")).await;
    assert!(resp.is_ok(), "sandbox_create should succeed: {:?}", resp);
    let data = resp.data.unwrap();
    assert_eq!(data["vm"], "pool-0");
    assert_eq!(data["pool"], true);

    // Slot is claimed with the synthesized layer set (system base only).
    let resp = execute(&mut mgr, Command::new("pool_list")).await;
    let slots = resp.data.unwrap()["pool"].as_array().unwrap().clone();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0]["claimed"], true);
    assert_eq!(slots[0]["layers"], serde_json::json!(["base"]));
    assert_eq!(slots[0]["net"], false);

    // The record carries pool_backed.
    let id = data["id"].as_str().unwrap();
    let resp = execute(&mut mgr, Command::new("sandbox_info").with_id(id)).await;
    assert_eq!(resp.data.unwrap()["pool_backed"], true);

    // Second sandbox of the same tenant reuses pool-0 (tenant indexing —
    // not the tenant-<t> name convention).
    let resp = execute(&mut mgr, sandbox_create_cmd("research")).await;
    assert!(resp.is_ok(), "second sandbox_create: {:?}", resp);
    let data = resp.data.unwrap();
    assert_eq!(data["vm"], "pool-0");
    assert_eq!(data["pool"], true);
    let resp = execute(&mut mgr, Command::new("pool_list")).await;
    assert_eq!(resp.data.unwrap()["count"], 1, "still exactly one slot");
}

/// No idle slot (pool exhausted) → cold-boot fallback: the tenant VM is
/// named tenant-<t> and the response says pool:false.
#[tokio::test]
async fn test_pool_exhausted_cold_fallback() {
    let mut mgr = make_mgr();
    create_pool(&mut mgr, 1).await;

    let resp = execute(&mut mgr, sandbox_create_cmd("a")).await;
    assert_eq!(resp.data.unwrap()["vm"], "pool-0");

    let resp = execute(&mut mgr, sandbox_create_cmd("b")).await;
    assert!(resp.is_ok(), "cold fallback should succeed: {:?}", resp);
    let data = resp.data.unwrap();
    assert_eq!(data["vm"], "tenant-b");
    assert_eq!(data["pool"], false);
}

/// net requirement mismatch → cold fallback; the idle slot stays idle.
#[tokio::test]
async fn test_net_mismatch_cold_fallback() {
    let mut mgr = make_mgr();
    create_pool(&mut mgr, 1).await; // slots boot with net:false

    let resp = execute(&mut mgr, sandbox_create_cmd("research").with_net(true)).await;
    assert!(resp.is_ok(), "cold fallback should succeed: {:?}", resp);
    let data = resp.data.unwrap();
    assert_eq!(data["vm"], "tenant-research");
    assert_eq!(data["pool"], false);

    let resp = execute(&mut mgr, Command::new("pool_list")).await;
    let slots = resp.data.unwrap()["pool"].as_array().unwrap().clone();
    assert_eq!(slots[0]["claimed"], false, "mismatched slot must stay idle");
}

/// "pool": false forbids the pool even when a matching slot is idle.
#[tokio::test]
async fn test_pool_forbidden_cold_boots() {
    let mut mgr = make_mgr();
    create_pool(&mut mgr, 1).await;

    let resp = execute(&mut mgr, sandbox_create_cmd("research").with_pool(false)).await;
    assert!(resp.is_ok(), "cold boot should succeed: {:?}", resp);
    let data = resp.data.unwrap();
    assert_eq!(data["vm"], "tenant-research");
    assert_eq!(data["pool"], false);
    let resp = execute(&mut mgr, Command::new("pool_list")).await;
    assert_eq!(resp.data.unwrap()["pool"][0]["claimed"], false);
}

/// tenant_destroy on a pool-backed tenant: the VM is released to the pool
/// (slot idle, layers cleared, VM still registered), records dropped.
#[tokio::test]
async fn test_tenant_destroy_pool_backed_releases() {
    let mut mgr = make_mgr();
    create_pool(&mut mgr, 1).await;
    execute(&mut mgr, sandbox_create_cmd("research")).await;

    let resp = execute(
        &mut mgr,
        Command::new("tenant_destroy").with_tenant("research"),
    )
    .await;
    assert!(resp.is_ok(), "tenant_destroy should succeed: {:?}", resp);
    let data = resp.data.unwrap();
    assert_eq!(data["released_to_pool"], true);
    assert_eq!(data["sandboxes_removed"], 1);
    assert_eq!(data["vm"], "pool-0");

    // Slot is idle again with cleared layers; the VM is still registered.
    let resp = execute(&mut mgr, Command::new("pool_list")).await;
    let slots = resp.data.unwrap()["pool"].as_array().unwrap().clone();
    assert_eq!(slots[0]["claimed"], false);
    assert_eq!(slots[0]["layers"], serde_json::json!([]));
    let resp = execute(&mut mgr, Command::new("info").with_name("pool-0")).await;
    assert!(resp.is_ok(), "pool VM must survive tenant_destroy");

    // Records are gone; the slot can serve a new tenant immediately.
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_list").with_tenant("research"),
    )
    .await;
    assert_eq!(resp.data.unwrap()["count"], 0);
    let resp = execute(&mut mgr, sandbox_create_cmd("other")).await;
    assert_eq!(resp.data.unwrap()["vm"], "pool-0");
}

/// tenant_destroy on a cold-booted tenant destroys the VM.
#[tokio::test]
async fn test_tenant_destroy_cold_destroys_vm() {
    let mut mgr = make_mgr();
    execute(&mut mgr, sandbox_create_cmd("research")).await;

    let resp = execute(
        &mut mgr,
        Command::new("tenant_destroy").with_tenant("research"),
    )
    .await;
    let data = resp.data.unwrap();
    assert_eq!(data["released_to_pool"], false);
    assert_eq!(data["status"], "destroyed");

    let resp = execute(&mut mgr, Command::new("info").with_name("tenant-research")).await;
    assert!(!resp.is_ok(), "cold tenant VM must be destroyed");
}

/// Destroying a pool-backed VM directly cascades: its sandbox records are
/// gone and the slot is removed (no dangling registry state).
#[tokio::test]
async fn test_destroy_pool_backed_vm_cascades() {
    let mut mgr = make_mgr();
    create_pool(&mut mgr, 1).await;
    execute(&mut mgr, sandbox_create_cmd("research")).await;

    let resp = execute(&mut mgr, Command::new("destroy").with_name("pool-0")).await;
    assert!(resp.is_ok(), "destroy should succeed: {:?}", resp);

    let resp = execute(&mut mgr, Command::new("sandbox_list")).await;
    assert_eq!(
        resp.data.unwrap()["count"],
        0,
        "sandbox records must not dangle after destroy"
    );
    let resp = execute(&mut mgr, Command::new("pool_list")).await;
    assert_eq!(resp.data.unwrap()["count"], 0, "slot must be removed");
}

/// pool_create waits for the guest agent before slating a slot idle:
/// with the first two pings failing, the slot appears only after the
/// third ping succeeds.
#[tokio::test]
async fn test_pool_create_waits_for_readiness() {
    let adapter = MockVmAdapter::new().with_ping_ready_after(2);
    let ping_count = adapter.ping_count();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into()).with_readiness_probe(10, 1);

    let resp = execute(&mut mgr, Command::new("pool_create").with_pool_size(1)).await;
    assert!(resp.is_ok(), "pool_create should succeed: {:?}", resp);
    assert_eq!(resp.data.unwrap()["created"], serde_json::json!(["pool-0"]));
    assert_eq!(
        *ping_count.lock().unwrap(),
        3,
        "two failed pings + one success"
    );
    let resp = execute(&mut mgr, Command::new("pool_list")).await;
    assert_eq!(resp.data.unwrap()["count"], 1);
}

/// A VM that never becomes ready is destroyed, never slotted, and
/// reported explicitly (error when nothing became ready).
#[tokio::test]
async fn test_pool_create_readiness_failure_is_honest() {
    let adapter = MockVmAdapter::new().with_ping_ready_after(u32::MAX);
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into()).with_readiness_probe(3, 1);

    let resp = execute(&mut mgr, Command::new("pool_create").with_pool_size(1)).await;
    assert!(!resp.is_ok(), "all-failed pool_create must error");
    assert!(
        resp.error.unwrap().contains("no pool VM became ready"),
        "error should name the readiness failure"
    );

    // Not slotted, not registered: the unready VM was destroyed.
    let resp = execute(&mut mgr, Command::new("pool_list")).await;
    assert_eq!(resp.data.unwrap()["count"], 0);
    let resp = execute(&mut mgr, Command::new("list")).await;
    assert_eq!(resp.data.unwrap()["count"], 0);
}

/// Partial readiness failure is reported alongside the ready VMs.
#[tokio::test]
async fn test_pool_create_partial_failure_reported() {
    // First VM never ready (fails 3 probes), second VM ready on ping 4.
    let adapter = MockVmAdapter::new().with_ping_ready_after(3);
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into()).with_readiness_probe(3, 1);

    let resp = execute(&mut mgr, Command::new("pool_create").with_pool_size(2)).await;
    assert!(resp.is_ok(), "partial success stays ok: {:?}", resp);
    let data = resp.data.unwrap();
    assert_eq!(data["created"], serde_json::json!(["pool-1"]));
    assert_eq!(data["failed"].as_array().unwrap().len(), 1);
    assert_eq!(data["failed"][0]["name"], "pool-0");
}

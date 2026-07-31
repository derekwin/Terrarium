//! Unit tests for VmManager using MockVmAdapter.
//!
//! Tests cover all major VmManager methods: spawn, exec, shutdown, kill,
//! destroy, list, reap, and net tracking. No real KVM or Cloud Hypervisor
//! is required — all tests use the pure in-memory MockVmAdapter.

mod common;

use std::sync::Arc;

use adapter_traits::{VmName, VmSpec};
use common::MockVmAdapter;

use terrarium_engine::manager::{SandboxRecord, VmManager};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a basic VmSpec for testing. Uses sensible defaults that pass
/// [`VmSpec::validate`].
fn test_spec(name: &str) -> VmSpec {
    VmSpec {
        name: VmName::new(name).unwrap(),
        kernel: "/fake/vmlinux".into(),
        cmdline: None,
        boot_vcpus: 1,
        max_vcpus: Some(4),
        memory_mb: 256,
        max_memory_mb: Some(1024),
        initramfs: None,
        net: false,
        fs: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_spawn_creates_vm() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("test-vm")).await.unwrap();
    let names = mgr.list_names();
    assert!(names.contains(&"test-vm"));
}

#[tokio::test]
async fn test_spawn_duplicate_name() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    let spec = test_spec("dup-vm");
    mgr.spawn(spec.clone()).await.unwrap();

    let result = mgr.spawn(spec).await;
    assert!(result.is_err(), "duplicate spawn should return error");
}

#[tokio::test]
async fn test_exec_delegates() {
    let adapter = Arc::new(MockVmAdapter::new().with_exec("hello\n", "", 0));
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("exec-vm")).await.unwrap();

    let result = mgr
        .exec(
            "exec-vm",
            &["echo".into(), "hello".into()],
            10,
            false,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn test_exec_not_found() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mgr = VmManager::new(adapter, "/tmp".into());

    let result = mgr
        .exec("nonexistent", &["ls".into()], 10, false, None, None)
        .await;
    assert!(result.is_err(), "exec on unknown VM should fail");
}

#[tokio::test]
async fn test_shutdown_removes() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("shutdown-vm")).await.unwrap();
    mgr.shutdown("shutdown-vm").await.unwrap();

    assert!(mgr.list_names().is_empty());
}

#[tokio::test]
async fn test_kill_removes() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("kill-vm")).await.unwrap();
    mgr.kill("kill-vm").await.unwrap();

    assert!(mgr.list_names().is_empty());
}

#[tokio::test]
async fn test_destroy_removes_and_cleans() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    let mut spec = test_spec("destroy-vm");
    spec.net = true;
    mgr.spawn(spec).await.unwrap();

    // Verify net tracking before destroy.
    assert!(mgr.has_net("destroy-vm"));
    assert_eq!(mgr.net_in_use(), 1);

    mgr.destroy("destroy-vm").await.unwrap();

    // VM removed from registry.
    assert!(mgr.list_names().is_empty());
    // Net tracking cleaned up.
    assert!(!mgr.has_net("destroy-vm"));
    assert_eq!(mgr.net_in_use(), 0);
}

/// Shutting down a warm-pool VM must drop its pool slot: a surviving slot
/// would claim a dead VM.
#[tokio::test]
async fn test_shutdown_cleans_pool_slot() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.pool_create(1, "/fake/vmlinux", "/fake/agent.cpio.gz", false)
        .await
        .unwrap();
    assert_eq!(mgr.pool_list().len(), 1);

    mgr.shutdown("pool-0").await.unwrap();

    assert!(mgr.pool_list().is_empty(), "pool slot must not dangle");
    assert!(mgr.list_names().is_empty());
}

/// Killing a pool VM must drop its slot and its net registration (a stale
/// net_vms entry would block net_down via net_in_use).
#[tokio::test]
async fn test_kill_cleans_pool_slot_and_net() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.pool_create(1, "/fake/vmlinux", "/fake/agent.cpio.gz", true)
        .await
        .unwrap();
    assert_eq!(mgr.pool_list().len(), 1);
    assert!(mgr.has_net("pool-0"));
    assert_eq!(mgr.net_in_use(), 1);

    mgr.kill("pool-0").await.unwrap();

    assert!(mgr.pool_list().is_empty(), "pool slot must not dangle");
    assert!(!mgr.has_net("pool-0"));
    assert_eq!(mgr.net_in_use(), 0);
}

/// Shutting down a net VM must drop its net registration too.
#[tokio::test]
async fn test_shutdown_cleans_net_tracking() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    let mut spec = test_spec("net-shutdown-vm");
    spec.net = true;
    mgr.spawn(spec).await.unwrap();
    assert_eq!(mgr.net_in_use(), 1);

    mgr.shutdown("net-shutdown-vm").await.unwrap();

    assert!(!mgr.has_net("net-shutdown-vm"));
    assert_eq!(mgr.net_in_use(), 0);
}

/// Destroying a VM drops its sandbox records and terminates any in-flight
/// background sessions on it (their VM is gone, so they can never
/// complete — they must not stay "running" forever).
#[tokio::test]
async fn test_destroy_cleans_sandboxes_and_sessions() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let adapter = Arc::new(
        MockVmAdapter::new()
            .with_alive(true)
            .with_exec_gate(gate.clone()),
    );
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("tenant-x")).await.unwrap();
    mgr.sandbox_insert(SandboxRecord {
        id: "sb-1".into(),
        tenant: "x".into(),
        vm_name: "tenant-x".into(),
        workdir: "/workdir/sb-1".into(),
        created_at: 0,
        policy: None,
        pool_backed: false,
    });
    let sid = "session-1".to_string();
    mgr.exec_background(
        "tenant-x",
        &["sleep".into(), "100".into()],
        60,
        true,
        &sid,
        Some("/workdir/sb-1"),
        Some("sb-1".into()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(mgr.session_status(&sid).unwrap().status, "running");

    mgr.destroy("tenant-x").await.unwrap();

    assert!(mgr.sandbox_list(None).is_empty(), "sandbox records must go");
    assert_eq!(
        mgr.session_status(&sid).unwrap().status,
        "terminated",
        "orphaned session must not stay running"
    );
    assert!(mgr
        .session_list()
        .iter()
        .all(|s| s.vm_name != "tenant-x" || s.status != "running"));

    gate.notify_one();
}

/// Reaping a dead VM drops its sandbox records too.
#[tokio::test]
async fn test_reap_dead_cleans_sandbox_records() {
    let adapter = Arc::new(MockVmAdapter::new().with_alive(false));
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("dead-vm")).await.unwrap();
    mgr.sandbox_insert(SandboxRecord {
        id: "sb-dead".into(),
        tenant: "t".into(),
        vm_name: "dead-vm".into(),
        workdir: "/workdir/sb-dead".into(),
        created_at: 0,
        policy: None,
        pool_backed: false,
    });

    let reaped = mgr.reap_dead();
    assert_eq!(reaped.len(), 1);

    assert!(mgr.sandbox_list(None).is_empty(), "sandbox records must go");
}

#[tokio::test]
async fn test_list_names() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("vm-a")).await.unwrap();
    mgr.spawn(test_spec("vm-b")).await.unwrap();

    let names = mgr.list_names();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"vm-a"));
    assert!(names.contains(&"vm-b"));
}

#[tokio::test]
async fn test_reap_dead() {
    let adapter = Arc::new(MockVmAdapter::new().with_alive(false));
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("dead-vm")).await.unwrap();

    let reaped = mgr.reap_dead();
    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0].as_ref(), "dead-vm");
    assert!(mgr.list_names().is_empty());
}

#[tokio::test]
async fn test_reap_live_vm_not_removed() {
    let adapter = Arc::new(MockVmAdapter::new().with_alive(true));
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("live-vm")).await.unwrap();

    let reaped = mgr.reap_dead();
    assert!(reaped.is_empty());
    assert!(mgr.list_names().contains(&"live-vm"));
}

#[tokio::test]
async fn test_net_tracking() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    let mut spec = test_spec("net-vm");
    spec.net = true;
    mgr.spawn(spec).await.unwrap();

    assert!(mgr.has_net("net-vm"));
    assert_eq!(mgr.net_in_use(), 1);

    // Create a second VM without net — should not affect tracking.
    mgr.spawn(test_spec("no-net-vm")).await.unwrap();
    assert!(!mgr.has_net("no-net-vm"));
    assert_eq!(mgr.net_in_use(), 1);
}

#[tokio::test]
async fn test_snapshot_dir() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mgr = VmManager::new(adapter, "/custom/snap".into());

    assert_eq!(mgr.snapshot_dir(), "/custom/snap");
}

#[tokio::test]
async fn test_shutdown_not_found() {
    let adapter = Arc::new(MockVmAdapter::new());
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    let result = mgr.shutdown("no-such-vm").await;
    assert!(result.is_err(), "shutdown of unknown VM should fail");
}

#[tokio::test]
async fn test_get_vm() {
    let adapter = Arc::new(MockVmAdapter::new().with_pid(42));
    let mut mgr = VmManager::new(adapter, "/tmp".into());

    mgr.spawn(test_spec("get-vm")).await.unwrap();

    let handle = mgr.get("get-vm");
    assert!(handle.is_some());
    assert_eq!(handle.unwrap().pid(), 42);

    // Unknown VM returns None.
    assert!(mgr.get("no-such-vm").is_none());
}

//! Unit tests for the command dispatch layer.
//!
//! Tests `terrarium_engine::commands::execute()` with various `Command`
//! structs against a `VmManager` backed by `MockVmAdapter`.

mod common;

use std::sync::Arc;

use adapter_traits::{
    Capability, DefaultAccess, Direction, Endpoint, FileAccess, PathPattern, ResourceLimits,
    SandboxPolicy,
};
use common::MockVmAdapter;
use terrarium_engine::commands::execute;
use terrarium_engine::manager::VmManager;
use terrarium_engine::policy::default_sandbox_policy;
use terrarium_protocol::Command;

// ---------------------------------------------------------------------------
// Helper: shared VmManager factory
// ---------------------------------------------------------------------------

/// Build a capability-based policy fixture (deny default, version 1).
fn make_policy(capabilities: Vec<Capability>, limits: ResourceLimits) -> SandboxPolicy {
    SandboxPolicy {
        capabilities,
        limits,
        default: DefaultAccess::Deny,
        audit: Default::default(),
        version: 1,
    }
}

/// Create a VmManager backed by a MockVmAdapter with exec configured
/// to return "hello world\n" stdout, "" stderr, exit code 0.
fn make_mgr() -> VmManager {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("hello world\n", "", 0);
    VmManager::new(Arc::new(adapter), "/tmp".into())
}

/// Create a VmManager with no exec pre-configuration (empty results).
fn make_mgr_empty() -> VmManager {
    VmManager::new(
        Arc::new(MockVmAdapter::new().with_state("Running")),
        "/tmp".into(),
    )
}

fn make_cmd(command: &str) -> Command {
    Command::new(command)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Create a valid VM → Response is ok with name/status/pid.
#[tokio::test]
async fn test_create_valid() {
    let mut mgr = make_mgr();
    let cmd = Command::create("test-vm", "/fake/vmlinux");
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "expected ok, got: {:?}", resp);
    let data = resp.data.expect("should have data");
    assert_eq!(data["name"], "test-vm");
    assert_eq!(data["status"], "created");
}

/// Create the same VM twice → second returns error.
#[tokio::test]
async fn test_create_duplicate() {
    let mut mgr = make_mgr();
    let cmd = Command::create("dup-vm", "/fake/vmlinux");
    let resp1 = execute(&mut mgr, cmd.clone()).await;
    assert!(resp1.is_ok(), "first create should succeed");

    let resp2 = execute(&mut mgr, cmd).await;
    assert!(!resp2.is_ok(), "duplicate create should fail");
    assert!(resp2.error.unwrap().contains("already exists"));
}

/// List with no VMs → count=0.
#[tokio::test]
async fn test_list_empty() {
    let mut mgr = make_mgr();
    let cmd = make_cmd("list");
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok());
    let data = resp.data.expect("should have data");
    assert_eq!(data["count"], 0);
}

/// List after creating a VM → count >= 1.
#[tokio::test]
async fn test_list_with_vms() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("vm-a", "/fake/vmlinux")).await;
    let cmd = make_cmd("list");
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok());
    let data = resp.data.expect("should have data");
    let count = data["count"].as_u64().expect("count should be u64");
    assert!(count >= 1, "expected at least 1 VM, got count={}", count);
}

/// Info on an existing VM → returns details.
#[tokio::test]
async fn test_info_found() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("info-vm", "/fake/vmlinux")).await;
    let cmd = make_cmd("info").with_name("info-vm");
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "info should succeed: {:?}", resp);
    let data = resp.data.expect("should have data");
    assert_eq!(data["name"], "info-vm");
    assert_eq!(data["state"], "Running");
}

/// Info on a nonexistent VM → error.
#[tokio::test]
async fn test_info_not_found() {
    let mut mgr = make_mgr();
    let cmd = Command::new("info").with_name("ghost");
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("not found"),
        "expected 'not found' error"
    );
}

/// Exec on an existing VM with arguments → returns output.
#[tokio::test]
async fn test_exec_valid() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    let cmd = Command::new("exec")
        .with_name("exec-vm")
        .with_args(vec!["echo".into(), "hello".into()]);
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "exec should succeed: {:?}", resp);
    let data = resp.data.expect("should have data");
    assert_eq!(data["stdout"], "hello world\n");
    assert_eq!(data["exit_code"], 0);
}

/// Exec with sandbox=true → flag is accepted and forwarded through the
/// dispatch layer (mock adapter ignores it; confinement itself is
/// guest-proxy's job, unit-tested there).
#[tokio::test]
async fn test_exec_sandbox_flag() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    let cmd = Command::new("exec")
        .with_name("exec-vm")
        .with_args(vec!["echo".into(), "hello".into()])
        .with_sandbox(true);
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "sandboxed exec should succeed: {:?}", resp);
    let data = resp.data.expect("should have data");
    assert_eq!(data["stdout"], "hello world\n");
}

/// Exec with policy + sandbox:true → policy is forwarded to the adapter
/// as base ∪ user: the engine default capability set is preserved and the
/// user's grants are appended on top (limits: user wins). This mirrors
/// `default_sandbox_policy().merged_with(&user)`.
#[tokio::test]
async fn test_exec_policy_forwarded() {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    let exec_log = adapter.exec_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    exec_log.lock().unwrap().clear();

    let policy = make_policy(
        vec![Capability::File {
            path: PathPattern::Prefix("/opt/data".into()),
            access: FileAccess::Read,
        }],
        ResourceLimits {
            memory_mb: Some(512),
            ..Default::default()
        },
    );
    let resp = execute(
        &mut mgr,
        Command::new("exec")
            .with_name("exec-vm")
            .with_args(vec!["echo".into(), "hello".into()])
            .with_sandbox(true)
            .with_policy(policy.clone()),
    )
    .await;
    assert!(resp.is_ok(), "policy exec should succeed: {:?}", resp);
    let log = exec_log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].sandbox);
    assert_eq!(
        log[0].policy.as_ref(),
        Some(&default_sandbox_policy().merged_with(&policy))
    );
}

/// D2 parity with sandbox_exec: a sandboxed `exec` (sandbox:true) with no
/// policy receives the engine default — the injection is gated on the
/// sandbox flag, not on the command name. An unsandboxed `exec` stays
/// policy-free.
#[tokio::test]
async fn test_exec_sandboxed_injects_default_policy() {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    let exec_log = adapter.exec_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    exec_log.lock().unwrap().clear();

    // sandbox:true + no policy → engine injects the default policy.
    let resp = execute(
        &mut mgr,
        Command::new("exec")
            .with_name("exec-vm")
            .with_args(vec!["echo".into(), "hi".into()])
            .with_sandbox(true),
    )
    .await;
    assert!(resp.is_ok(), "sandboxed exec should succeed: {:?}", resp);
    {
        let log = exec_log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].sandbox);
        assert_eq!(
            log[0].policy.as_ref(),
            Some(&terrarium_engine::policy::default_sandbox_policy()),
            "sandboxed exec with no policy must receive the engine default"
        );
    }

    // sandbox:false + no policy → policy stays None (escape hatch).
    let resp = execute(
        &mut mgr,
        Command::new("exec")
            .with_name("exec-vm")
            .with_args(vec!["echo".into(), "hi".into()])
            .with_sandbox(false),
    )
    .await;
    assert!(resp.is_ok(), "unsandboxed exec should succeed: {:?}", resp);
    {
        let log = exec_log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(!log[1].sandbox);
        assert_eq!(
            log[1].policy, None,
            "unsandboxed exec must stay policy-free"
        );
    }
}

/// Exec with a policy but sandbox:false/absent → explicit error.
#[tokio::test]
async fn test_exec_policy_requires_sandbox() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    for sandbox in [None, Some(false)] {
        let mut cmd = Command::new("exec")
            .with_name("exec-vm")
            .with_args(vec!["echo".into(), "hello".into()])
            .with_policy(make_policy(
                vec![Capability::File {
                    path: PathPattern::Prefix("/opt/data".into()),
                    access: FileAccess::Read,
                }],
                ResourceLimits::default(),
            ));
        if let Some(s) = sandbox {
            cmd = cmd.with_sandbox(s);
        }
        let resp = execute(&mut mgr, cmd).await;
        assert!(!resp.is_ok(), "sandbox {:?} + policy must fail", sandbox);
        assert!(
            resp.error
                .unwrap()
                .contains("'policy' requires sandboxed exec"),
            "error should name the policy/sandbox conflict"
        );
    }
}

/// Exec on a nonexistent VM → error.
#[tokio::test]
async fn test_exec_unknown_vm() {
    let mut mgr = make_mgr();
    let cmd = Command::new("exec")
        .with_name("nonexistent")
        .with_args(vec!["ls".into()]);
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok());
    assert!(resp.error.unwrap().contains("not found"));
}

/// Exec with empty args → error.
#[tokio::test]
async fn test_exec_empty_args() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    let cmd = Command::new("exec").with_name("exec-vm"); // no args
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok());
    assert!(resp.error.unwrap().contains("Missing 'args'"));
}

/// Create with layers=["python312"] → base is auto-appended, create succeeds.
#[tokio::test]
async fn test_auto_append_base() {
    let mut mgr = make_mgr();
    let cmd = Command::create("layered-vm", "/fake/vmlinux").with_layers(vec!["python312".into()]);
    let resp = execute(&mut mgr, cmd).await;
    assert!(
        resp.is_ok(),
        "create with layers should succeed (base auto-appended): {:?}",
        resp
    );
    let data = resp.data.expect("should have data");
    assert_eq!(data["name"], "layered-vm");
}

/// net_up command → dispatch works (may fail without root, but not a crash).
#[tokio::test]
async fn test_net_up() {
    let mut mgr = make_mgr();
    let cmd = make_cmd("net_up");
    let resp = execute(&mut mgr, cmd).await;
    // Dispatch must not panic; response may be ok or error depending on
    // whether the test has root for creating a real NAT bridge.
    assert!(
        resp.status == "ok" || resp.status == "error",
        "net_up should dispatch without panic"
    );
}

/// pool_create with pool_size=1 → returns created names.
#[tokio::test]
async fn test_pool_create() {
    let mut mgr = make_mgr_empty();
    let cmd = Command::new("pool_create").with_pool_size(1);
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "pool_create should succeed: {:?}", resp);
    let data = resp.data.expect("should have data");
    let created = data["created"].as_array().expect("created should be array");
    assert_eq!(created.len(), 1);
    assert_eq!(data["count"], 1);
}

/// pool_create with size=0 → error.
#[tokio::test]
async fn test_pool_create_zero_size() {
    let mut mgr = make_mgr();
    let cmd = Command::new("pool_create").with_pool_size(0);
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok());
    assert!(resp.error.unwrap().contains("pool_size"));
}

/// pool_claim with layers → returns claimed name.
#[tokio::test]
async fn test_pool_claim() {
    let mut mgr = make_mgr_empty();
    // First create a pool with 1 VM
    let mut pool_cmd = Command::new("pool_create");
    pool_cmd.kernel = Some("/fake/vmlinux".into());
    pool_cmd.pool_size = Some(1);
    execute(&mut mgr, pool_cmd).await;
    let cmd = Command::new("pool_claim").with_layers(vec!["python312".into()]);
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "pool_claim should succeed: {:?}", resp);
    let data = resp.data.expect("should have data");
    assert!(
        data["name"].as_str().unwrap().starts_with("pool-"),
        "claimed name should start with 'pool-'"
    );
}

/// pool_release on a claimed VM → returns ok.
#[tokio::test]
async fn test_pool_release() {
    let mut mgr = make_mgr_empty();
    let mut pool_cmd = Command::new("pool_create");
    pool_cmd.kernel = Some("/fake/vmlinux".into());
    pool_cmd.pool_size = Some(1);
    execute(&mut mgr, pool_cmd).await;
    let claim_resp = execute(
        &mut mgr,
        Command::new("pool_claim").with_layers(vec!["python312".into()]),
    )
    .await;
    let claimed_name = claim_resp.data.unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    let cmd = Command::new("pool_release").with_name(&claimed_name);
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "pool_release should succeed: {:?}", resp);
}

/// snapshot command on an existing VM → default managed path
/// ({snapshot_dir}/terra-snap-<vm>.bin).
#[tokio::test]
async fn test_snapshot() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("snap-vm", "/fake/vmlinux")).await;
    let cmd = Command::new("snapshot").with_name("snap-vm");
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "snapshot should succeed: {:?}", resp);
    assert!(
        resp.data.unwrap()["snapshot_path"] == "/tmp/terra-snap-snap-vm",
        "default snapshot path should be {{snapshot_dir}}/terra-snap-<vm> (a directory)"
    );
}

/// Unknown command → error.
#[tokio::test]
async fn test_unknown_command() {
    let mut mgr = make_mgr();
    let cmd = Command::new("nonexistent_cmd");
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("Unknown command"),
        "should return 'Unknown command' error"
    );
}

// ---------------------------------------------------------------------------
// Additional coverage: destroy + shutdown + resize + info missing name
// ---------------------------------------------------------------------------

/// Destroy a VM → VM is stopped and deregistered, info on it returns not-found.
#[tokio::test]
async fn test_destroy() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("killme", "/fake/vmlinux")).await;
    let cmd = Command::new("destroy").with_name("killme");
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "destroy should succeed: {:?}", resp);

    // Destroy is "stop + deregister", so info should fail after.
    let info_resp = execute(&mut mgr, Command::new("info").with_name("killme")).await;
    assert!(!info_resp.is_ok());
}

/// shutdown + kill follow the same "stop + deregister" semantics.
#[tokio::test]
async fn test_shutdown_and_kill() {
    let mut mgr = make_mgr();

    // shutdown
    execute(&mut mgr, Command::create("sd-vm", "/fake/vmlinux")).await;
    let sd = execute(&mut mgr, Command::new("shutdown").with_name("sd-vm")).await;
    assert!(sd.is_ok(), "shutdown should succeed: {:?}", sd);

    // kill
    execute(&mut mgr, Command::create("k-vm", "/fake/vmlinux")).await;
    let k = execute(&mut mgr, Command::new("kill").with_name("k-vm")).await;
    assert!(k.is_ok(), "kill should succeed: {:?}", k);
}

/// resize an existing VM → ok.
#[tokio::test]
async fn test_resize() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("resize-me", "/fake/vmlinux")).await;
    let cmd = Command::new("resize")
        .with_name("resize-me")
        .with_cpus(4)
        .with_memory_bytes(1024 * 1024 * 1024);
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "resize should succeed: {:?}", resp);
}

/// resize with fewer cpus than the VM currently has → explicit unsupported
/// error (CH vCPU removal requires guest-side offlining; guest-proxy only
/// ever ONLINES hot-added vCPUs). Never forwarded to CH.
#[tokio::test]
async fn test_resize_cpu_shrink_rejected() {
    let adapter = MockVmAdapter::new().with_state("Running").with_cpus(4);
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());
    execute(&mut mgr, Command::create("shrink-me", "/fake/vmlinux")).await;
    let cmd = Command::new("resize").with_name("shrink-me").with_cpus(2);
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok(), "cpu shrink must fail: {:?}", resp);
    assert!(
        resp.error.unwrap().contains("CPU shrink"),
        "error should name CPU shrink as unsupported"
    );
}

/// resize with more cpus than the VM currently has → proceeds (mock resize
/// succeeds), hot-add is supported.
#[tokio::test]
async fn test_resize_cpu_grow_proceeds() {
    let adapter = MockVmAdapter::new().with_state("Running").with_cpus(4);
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());
    execute(&mut mgr, Command::create("grow-me", "/fake/vmlinux")).await;
    let cmd = Command::new("resize").with_name("grow-me").with_cpus(6);
    let resp = execute(&mut mgr, cmd).await;
    assert!(resp.is_ok(), "cpu grow should succeed: {:?}", resp);
}

/// resize with only memory_bytes (no cpus) → proceeds unchanged; memory
/// shrink via virtio-mem is supported (guest driver handles unplug).
#[tokio::test]
async fn test_resize_memory_only_proceeds() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("mem-me", "/fake/vmlinux")).await;
    let cmd = Command::new("resize")
        .with_name("mem-me")
        .with_memory_bytes(512 * 1024 * 1024);
    let resp = execute(&mut mgr, cmd).await;
    assert!(
        resp.is_ok(),
        "memory-only resize should succeed: {:?}",
        resp
    );
}

/// info without a name → error.
#[tokio::test]
async fn test_info_missing_name() {
    let mut mgr = make_mgr();
    let cmd = Command::new("info"); // no name set
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("Missing 'name'"),
        "should return 'Missing name' error"
    );
}

// ---------------------------------------------------------------------------
// Errors are explicit: no fake success / silent fallback
// ---------------------------------------------------------------------------

/// session_kill on an unknown session → explicit not-found error.
#[tokio::test]
async fn test_session_kill_not_found() {
    let mut mgr = make_mgr();
    let cmd = Command::new("session_kill").with_session_id("some-session");
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("not found"),
        "session_kill of unknown session should return not-found"
    );
}

/// session_kill of a running background session: a real kill is issued to
/// the guest (with the session id as exec_id) and the session ends up
/// "killed" — the completion path must not overwrite that status.
#[tokio::test]
async fn test_session_kill_running_session() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec_gate(gate.clone());
    let kill_log = adapter.kill_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    let resp = execute(
        &mut mgr,
        Command::new("exec")
            .with_name("exec-vm")
            .with_args(vec!["sleep".into(), "100".into()])
            .with_exec_mode("background"),
    )
    .await;
    assert!(resp.is_ok(), "background exec should start: {:?}", resp);
    let session_id = resp.data.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = execute(
        &mut mgr,
        Command::new("session_kill").with_session_id(&session_id),
    )
    .await;
    assert!(resp.is_ok(), "session_kill should succeed: {:?}", resp);

    // The guest received a kill for this exact exec_id (the session id).
    assert_eq!(
        kill_log.lock().unwrap().as_slice(),
        std::slice::from_ref(&session_id)
    );

    // Let the parked exec finish; the completion path must NOT overwrite
    // the "killed" status with completed/failed.
    gate.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let resp = execute(
        &mut mgr,
        Command::new("session_status").with_session_id(&session_id),
    )
    .await;
    assert_eq!(resp.data.unwrap()["status"], "killed");
}

/// Destroying a VM terminates its orphaned sessions; session_kill of such
/// a session → explicit error (never fake success).
#[tokio::test]
async fn test_session_kill_vm_gone() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec_gate(gate.clone());
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    let resp = execute(
        &mut mgr,
        Command::new("exec")
            .with_name("exec-vm")
            .with_args(vec!["sleep".into(), "100".into()])
            .with_exec_mode("background"),
    )
    .await;
    let session_id = resp.data.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    execute(&mut mgr, Command::new("destroy").with_name("exec-vm")).await;
    // Destroy must terminate the orphaned session, not leave it "running".
    let resp = execute(
        &mut mgr,
        Command::new("session_status").with_session_id(&session_id),
    )
    .await;
    assert_eq!(resp.data.unwrap()["status"], "terminated");
    let resp = execute(
        &mut mgr,
        Command::new("session_kill").with_session_id(&session_id),
    )
    .await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("not running"),
        "killing a terminated session should error explicitly"
    );
    gate.notify_one();
}

/// Unknown exec_mode → invalid-argument error (no silent blocking fallback).
#[tokio::test]
async fn test_exec_invalid_mode() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    let cmd = Command::new("exec")
        .with_name("exec-vm")
        .with_args(vec!["ls".into()])
        .with_exec_mode("detached");
    let resp = execute(&mut mgr, cmd).await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("invalid exec_mode"),
        "unknown exec_mode should be rejected"
    );
}

/// Explicit blocking exec_mode still works.
#[tokio::test]
async fn test_exec_explicit_blocking_mode() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    let cmd = Command::new("exec")
        .with_name("exec-vm")
        .with_args(vec!["ls".into()])
        .with_exec_mode("blocking");
    let resp = execute(&mut mgr, cmd).await;
    assert!(
        resp.is_ok(),
        "explicit blocking mode should work: {:?}",
        resp
    );
}

/// Custom snapshot_path is honored (P1: named/managed snapshots).
#[tokio::test]
async fn test_snapshot_custom_path_honored() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("snap-vm", "/fake/vmlinux")).await;
    let cmd = Command::new("snapshot")
        .with_name("snap-vm")
        .with_snapshot_path("/tmp/custom.snap");
    let resp = execute(&mut mgr, cmd).await;
    assert!(
        resp.is_ok(),
        "custom snapshot_path should be honored: {:?}",
        resp
    );
    assert!(
        resp.data.unwrap()["snapshot_path"] == "/tmp/custom.snap",
        "snapshot_path should round-trip"
    );
}

/// restore creates a NEW VM registered under the given name (P1 fast
/// reset): the engine sees a normal registered VM; the guest state comes
/// from the snapshot.
#[tokio::test]
async fn test_restore_creates_registered_vm() {
    let mut mgr = make_mgr();
    let resp = execute(
        &mut mgr,
        Command::new("restore")
            .with_name("restored-vm")
            .with_snapshot_path("/tmp/terra-snap-env.bin")
            .with_cpus(2)
            .with_memory_mb(512),
    )
    .await;
    assert!(resp.is_ok(), "restore should succeed: {:?}", resp);
    let data = resp.data.unwrap();
    assert_eq!(data["name"], "restored-vm");
    assert_eq!(data["status"], "restored");

    // The restored VM is registered and inspectable.
    let info = execute(&mut mgr, Command::new("info").with_name("restored-vm")).await;
    assert!(info.is_ok(), "restored VM should be registered: {:?}", info);
}

/// restore without snapshot_path → explicit error.
#[tokio::test]
async fn test_restore_requires_snapshot_path() {
    let mut mgr = make_mgr();
    let resp = execute(&mut mgr, Command::new("restore").with_name("restored-vm")).await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("snapshot_path"),
        "restore without snapshot_path must error"
    );
}

/// daemon_stop via execute() (non-daemon path) → explicit error; the real
/// handling lives in the daemon listener (see daemon_tests.rs).
#[tokio::test]
async fn test_daemon_stop_outside_daemon() {
    let mut mgr = make_mgr();
    let resp = execute(&mut mgr, Command::new("daemon_stop")).await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("daemon listener"),
        "daemon_stop outside the daemon should be an explicit error"
    );
}

/// Exec with a Network capability that has an empty host → explicit error
/// (validate() rejects nameless endpoints). The old "empty net_allow"
/// footgun is gone — an empty capability list is valid default-deny.
#[tokio::test]
async fn test_exec_policy_network_empty_host_rejected() {
    let mut mgr = make_mgr();
    execute(&mut mgr, Command::create("exec-vm", "/fake/vmlinux")).await;
    let resp = execute(
        &mut mgr,
        Command::new("exec")
            .with_name("exec-vm")
            .with_args(vec!["echo".into(), "hello".into()])
            .with_sandbox(true)
            .with_policy(make_policy(
                vec![Capability::Network {
                    endpoint: Endpoint {
                        host: String::new(),
                        port: None,
                    },
                    direction: Direction::Outbound,
                }],
                ResourceLimits::default(),
            )),
    )
    .await;
    assert!(!resp.is_ok(), "empty network host must fail");
    assert!(
        resp.error
            .unwrap()
            .contains("network endpoint host must not be empty"),
        "error should name the endpoint constraint"
    );
}

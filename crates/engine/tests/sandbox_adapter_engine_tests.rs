//! C3 wiring tests: the engine's sandbox lifecycle routes through the
//! `SandboxAdapter` — `sandbox_create` binds a handle (effective policy),
//! blocking `sandbox_exec` goes through `SandboxHandle::exec`, while
//! background sessions and the unsandboxed escape hatch keep the direct
//! `vm.exec` path. A `MockSandboxAdapter` (tests/common) records the
//! create/exec/destroy calls the engine makes.

mod common;

use std::sync::Arc;

use adapter_traits::{
    Capability, DefaultAccess, FileAccess, PathPattern, ResourceLimits, SandboxPolicy,
};
use common::{MockSandboxAdapter, MockVmAdapter};
use terrarium_engine::commands::execute;
use terrarium_engine::manager::VmManager;
use terrarium_engine::policy::default_sandbox_policy;
use terrarium_protocol::Command;

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

/// A VmManager backed by a mock VM adapter plus a mock sandbox adapter.
fn make_mgr(sandbox: MockSandboxAdapter) -> VmManager {
    let vm = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    VmManager::new(Arc::new(vm), "/tmp".into()).with_sandbox_adapter(Box::new(sandbox))
}

/// Create a sandbox through the command layer; returns its id.
async fn create_sandbox(mgr: &mut VmManager, tenant: &str) -> String {
    let resp = execute(
        mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant(tenant),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_create: {:?}", resp);
    resp.data.unwrap()["id"].as_str().unwrap().to_string()
}

/// C3: sandbox_create calls `SandboxAdapter::create` with the effective
/// policy (engine default ∪ user) and stores the bound handle in the record.
#[tokio::test]
async fn test_sandbox_create_binds_handle_with_effective_policy() {
    let sandbox = MockSandboxAdapter::new();
    let create_log = sandbox.create_log();
    let mut mgr = make_mgr(sandbox);

    let user = make_policy(
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
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_policy(user.clone()),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_create: {:?}", resp);
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();

    // The adapter saw the effective (default ∪ user) policy, not the raw one.
    let log = create_log.lock().unwrap();
    assert_eq!(log.len(), 1, "one create call");
    assert_eq!(
        log[0].spec.policy.as_ref(),
        Some(&default_sandbox_policy().merged_with(&user)),
        "create must bind the effective policy"
    );

    // The record holds the returned handle.
    assert!(
        mgr.sandbox_get(&id).unwrap().handle.is_some(),
        "record must hold the bound session handle"
    );
}

/// C3: a sandbox created without a policy binds the engine default.
#[tokio::test]
async fn test_sandbox_create_without_policy_binds_default() {
    let sandbox = MockSandboxAdapter::new();
    let create_log = sandbox.create_log();
    let mut mgr = make_mgr(sandbox);

    let id = create_sandbox(&mut mgr, "research").await;

    let log = create_log.lock().unwrap();
    assert_eq!(log[0].spec.policy.as_ref(), Some(&default_sandbox_policy()));
    assert!(mgr.sandbox_get(&id).unwrap().handle.is_some());
}

/// C3: a sandboxed blocking sandbox_exec routes through the bound handle —
/// the mock handle receives args, workdir, the per-call override and the
/// clamped timeout, and its result becomes the response. The direct
/// `vm.exec` path is not used for the sandboxed call.
#[tokio::test]
async fn test_sandbox_exec_blocking_routes_through_handle() {
    let sandbox = MockSandboxAdapter::new().with_exec("out\n", "", 0);
    let handle_log = sandbox.exec_log();
    let vm = Arc::new(
        MockVmAdapter::new()
            .with_state("Running")
            .with_exec("ok\n", "", 0),
    );
    let vm_log = vm.exec_log();
    let mut mgr = VmManager::new(vm, "/tmp".into()).with_sandbox_adapter(Box::new(sandbox));

    let id = create_sandbox(&mut mgr, "research").await;
    vm_log.lock().unwrap().clear(); // drop the mkdir call

    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()])
            .with_timeout_secs(42),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_exec: {:?}", resp);
    assert_eq!(resp.data.unwrap()["stdout"], "out\n");

    // The blocking exec never reached the VM directly.
    assert!(
        vm_log.lock().unwrap().is_empty(),
        "blocking sandbox_exec must not use the direct vm.exec path"
    );

    let log = handle_log.lock().unwrap();
    assert_eq!(log.len(), 1, "one handle.exec call");
    assert_eq!(log[0].args, vec!["echo", "hi"]);
    assert_eq!(
        log[0].work_dir.as_deref(),
        Some(format!("/workdir/{}", id)).as_deref()
    );
    assert_eq!(
        log[0].timeout_secs,
        Some(42),
        "clamped timeout passes through"
    );
}

/// The workdir is the session's /workdir/<id> — asserted separately from
/// the handle routing test above (the id is random per create).
#[tokio::test]
async fn test_sandbox_exec_blocking_handle_gets_workdir() {
    let sandbox = MockSandboxAdapter::new().with_exec("out\n", "", 0);
    let handle_log = sandbox.exec_log();
    let mut mgr = make_mgr(sandbox);

    let id = create_sandbox(&mut mgr, "research").await;
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["pwd".into()]),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_exec: {:?}", resp);

    let log = handle_log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(
        log[0].work_dir.as_deref(),
        Some(format!("/workdir/{}", id)).as_deref()
    );
}

/// C3: the engine passes only the per-call override on the handle — the
/// stored policy stays bound at create, and a call without an override
/// carries `None` (the bound policy applies).
#[tokio::test]
async fn test_sandbox_exec_blocking_override_precedence_through_handle() {
    let sandbox = MockSandboxAdapter::new();
    let handle_log = sandbox.exec_log();
    let mut mgr = make_mgr(sandbox);

    let stored = make_policy(
        vec![Capability::File {
            path: PathPattern::Prefix("/opt/data".into()),
            access: FileAccess::Read,
        }],
        ResourceLimits {
            memory_mb: Some(256),
            ..Default::default()
        },
    );
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_policy(stored),
    )
    .await;
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();

    let override_policy = make_policy(
        vec![],
        ResourceLimits {
            memory_mb: Some(1024),
            ..Default::default()
        },
    );
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()])
            .with_policy(override_policy.clone()),
    )
    .await;
    assert!(resp.is_ok(), "override exec: {:?}", resp);
    {
        let log = handle_log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0].policy_override.as_ref(),
            Some(&override_policy),
            "the raw per-call override is passed through (never merged/stored)"
        );
    }

    // Next call without an override → the handle sees None (bound policy).
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()]),
    )
    .await;
    assert!(resp.is_ok());
    let log = handle_log.lock().unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].policy_override, None);
}

/// C3: an invalid per-call override is still rejected before routing
/// (validation happens in the engine, exactly as before C3).
#[tokio::test]
async fn test_sandbox_exec_blocking_rejects_invalid_override() {
    let sandbox = MockSandboxAdapter::new();
    let handle_log = sandbox.exec_log();
    let mut mgr = make_mgr(sandbox);

    let id = create_sandbox(&mut mgr, "research").await;
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()])
            .with_policy(make_policy(
                vec![Capability::File {
                    path: PathPattern::Prefix("relative/path".into()),
                    access: FileAccess::Read,
                }],
                ResourceLimits::default(),
            )),
    )
    .await;
    assert!(!resp.is_ok(), "invalid override must fail");
    assert!(resp.error.unwrap().contains("must be absolute"));
    assert!(
        handle_log.lock().unwrap().is_empty(),
        "the handle must never see an invalid override"
    );
}

/// C3: background sandbox_exec keeps the direct vm.exec path (exec_id
/// registration for session_kill) — the mock handle is untouched.
#[tokio::test]
async fn test_sandbox_exec_background_stays_direct() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let vm = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0)
        .with_exec_gate(gate.clone());
    let vm_log = vm.exec_log();
    let sandbox = MockSandboxAdapter::new();
    let handle_log = sandbox.exec_log();
    let mut mgr =
        VmManager::new(Arc::new(vm), "/tmp".into()).with_sandbox_adapter(Box::new(sandbox));

    let id = create_sandbox(&mut mgr, "research").await;
    vm_log.lock().unwrap().clear();

    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["sleep".into(), "100".into()])
            .with_exec_mode("background"),
    )
    .await;
    assert!(resp.is_ok(), "background sandbox_exec: {:?}", resp);
    let session_id = resp.data.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The guest exec was registered under the session id (direct path).
    let mut exec_call = None;
    for _ in 0..100 {
        if let Some(c) = vm_log.lock().unwrap().iter().find(|c| c.args[0] == "sleep") {
            exec_call = Some(c.clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let exec_call = exec_call.expect("background exec should reach the guest");
    assert_eq!(exec_call.exec_id.as_deref(), Some(session_id.as_str()));

    assert!(
        handle_log.lock().unwrap().is_empty(),
        "background sessions must not route through the sandbox handle"
    );

    gate.notify_one();
}

/// C3: the unsandboxed escape hatch (`sandbox:false`) keeps the direct
/// vm.exec path, policy-free.
#[tokio::test]
async fn test_sandbox_exec_unsandboxed_stays_direct() {
    let vm = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    let vm_log = vm.exec_log();
    let sandbox = MockSandboxAdapter::new();
    let handle_log = sandbox.exec_log();
    let mut mgr =
        VmManager::new(Arc::new(vm), "/tmp".into()).with_sandbox_adapter(Box::new(sandbox));

    let id = create_sandbox(&mut mgr, "research").await;
    vm_log.lock().unwrap().clear();

    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()])
            .with_sandbox(false),
    )
    .await;
    assert!(resp.is_ok(), "unsandboxed sandbox_exec: {:?}", resp);
    let log = vm_log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert!(!log[0].sandbox);
    assert_eq!(log[0].policy, None);
    assert!(
        handle_log.lock().unwrap().is_empty(),
        "sandbox:false must not route through the handle"
    );
}

/// C3: sandbox_kill best-effort destroys the bound handle.
#[tokio::test]
async fn test_sandbox_kill_calls_handle_destroy() {
    let sandbox = MockSandboxAdapter::new();
    let destroy_count = sandbox.destroy_count();
    let mut mgr = make_mgr(sandbox);

    let id = create_sandbox(&mut mgr, "research").await;
    assert_eq!(*destroy_count.lock().unwrap(), 0);

    let resp = execute(&mut mgr, Command::new("sandbox_kill").with_id(&id)).await;
    assert!(resp.is_ok(), "sandbox_kill: {:?}", resp);
    assert_eq!(
        *destroy_count.lock().unwrap(),
        1,
        "sandbox_kill must destroy the bound handle"
    );
}

/// C3: tenant_destroy best-effort destroys every tenant sandbox's handle.
#[tokio::test]
async fn test_tenant_destroy_calls_handle_destroy() {
    let sandbox = MockSandboxAdapter::new();
    let destroy_count = sandbox.destroy_count();
    let mut mgr = make_mgr(sandbox);

    create_sandbox(&mut mgr, "research").await;
    create_sandbox(&mut mgr, "research").await;
    assert_eq!(*destroy_count.lock().unwrap(), 0);

    let resp = execute(
        &mut mgr,
        Command::new("tenant_destroy").with_tenant("research"),
    )
    .await;
    assert!(resp.is_ok(), "tenant_destroy: {:?}", resp);
    assert_eq!(
        *destroy_count.lock().unwrap(),
        2,
        "tenant_destroy must destroy every bound handle"
    );
}

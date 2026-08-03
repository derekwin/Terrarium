//! Tests for the engine-level sandbox commands (S-M2):
//! sandbox_create / sandbox_exec / sandbox_list / sandbox_info /
//! sandbox_kill / tenant_destroy, against a MockVmAdapter.

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

fn make_mgr() -> VmManager {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    VmManager::new(Arc::new(adapter), "/tmp".into())
}

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

/// sandbox_create: allocates sb-<hex>, VM "tenant-<tenant>", workdir under
/// /workdir, and creates the workdir in the guest (mkdir -p, unsandboxed).
#[tokio::test]
async fn test_sandbox_create() {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    let exec_log = adapter.exec_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research"),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_create should succeed: {:?}", resp);
    let data = resp.data.unwrap();
    let id = data["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("sb-"), "id should be sb-<hex>: {}", id);
    assert_eq!(data["vm"], "tenant-research");
    assert_eq!(data["workdir"], format!("/workdir/{}", id));

    // The tenant VM exists now.
    let info = execute(&mut mgr, Command::new("info").with_name("tenant-research")).await;
    assert!(info.is_ok(), "tenant VM should exist: {:?}", info);

    // The workdir was created in the guest, unsandboxed.
    {
        let log = exec_log.lock().unwrap();
        let mkdir = log
            .iter()
            .find(|c| c.args[0] == "mkdir")
            .expect("mkdir call");
        assert_eq!(mkdir.args, vec!["mkdir", "-p", &format!("/workdir/{}", id)]);
        assert!(!mkdir.sandbox, "workdir creation must be unsandboxed");
    }

    // The record is listed.
    let resp = execute(&mut mgr, Command::new("sandbox_info").with_id(&id)).await;
    assert!(resp.is_ok());
    let data = resp.data.unwrap();
    assert_eq!(data["tenant"], "research");
    assert_eq!(data["vm"], "tenant-research");
}

/// sandbox_create survives a briefly-unresponsive guest agent: the
/// workdir mkdir exec retries on vsock/handshake failures (slow-layer
/// boot race), so creation does not fail just because CH reported ready
/// a moment before the guest listener was up.
#[tokio::test]
async fn test_sandbox_create_retries_agent_boot_race() {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec_failures(2)
        .with_exec("ok\n", "", 0);
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("race"),
    )
    .await;
    assert!(
        resp.is_ok(),
        "create must retry through transient handshake failures: {:?}",
        resp
    );
}

/// sandbox_create is idempotent at the VM level: a second create for the
/// same tenant reuses the VM (no kernel needed, no duplicate-VM error) and
/// allocates a fresh sandbox id.
#[tokio::test]
async fn test_sandbox_create_idempotent_vm_reuse() {
    let mut mgr = make_mgr();
    let first = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research"),
    )
    .await;
    assert!(first.is_ok(), "first create: {:?}", first);

    // No kernel field: only works when the tenant VM is reused.
    let second = execute(
        &mut mgr,
        Command::new("sandbox_create").with_tenant("research"),
    )
    .await;
    assert!(
        second.is_ok(),
        "second create should reuse VM: {:?}",
        second
    );
    assert_ne!(
        first.data.unwrap()["id"],
        second.data.unwrap()["id"],
        "each create allocates a fresh sandbox id"
    );
}

/// Tenant names go into the VM name, so they must pass the VmName whitelist.
#[tokio::test]
async fn test_sandbox_create_invalid_tenant() {
    let mut mgr = make_mgr();
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("bad/name"),
    )
    .await;
    assert!(!resp.is_ok());
    assert!(
        resp.error.unwrap().contains("invalid tenant"),
        "bad tenant should be rejected"
    );
}

/// sandbox_exec: resolves the workdir from the registry and defaults to
/// sandboxed execution (absent flag → true).
#[tokio::test]
async fn test_sandbox_exec_defaults_sandboxed_with_workdir() {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    let exec_log = adapter.exec_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research"),
    )
    .await;
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();
    exec_log.lock().unwrap().clear();

    // Default: sandboxed.
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()]),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_exec should succeed: {:?}", resp);
    assert_eq!(resp.data.unwrap()["stdout"], "ok\n");
    {
        let log = exec_log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].sandbox, "sandbox must default to true");
        assert_eq!(
            log[0].work_dir.as_deref(),
            Some(format!("/workdir/{}", id)).as_deref()
        );
    }

    // Explicit escape hatch is honored.
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()])
            .with_sandbox(false),
    )
    .await;
    assert!(resp.is_ok());
    assert!(!exec_log.lock().unwrap()[1].sandbox);
}

/// sandbox_exec in background mode registers the session under the sandbox
/// and passes the session id down as the guest exec_id.
#[tokio::test]
async fn test_sandbox_exec_background_links_session() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0)
        .with_exec_gate(gate.clone());
    let exec_log = adapter.exec_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research"),
    )
    .await;
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();

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

    // Session carries the sandbox linkage.
    let resp = execute(
        &mut mgr,
        Command::new("session_status").with_session_id(&session_id),
    )
    .await;
    let data = resp.data.unwrap();
    assert_eq!(data["sandbox"], id);
    assert_eq!(data["status"], "running");

    // The guest exec was registered under the session id as exec_id.
    // (Poll: the spawned task pushes the log entry asynchronously.)
    let mut exec_call = None;
    for _ in 0..100 {
        if let Some(c) = exec_log
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.args[0] == "sleep")
        {
            exec_call = Some(c.clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let exec_call = exec_call.expect("background exec should reach the guest");
    assert_eq!(exec_call.exec_id.as_deref(), Some(session_id.as_str()));

    gate.notify_one();
}

/// Unknown sandbox ids fail loudly everywhere.
#[tokio::test]
async fn test_unknown_sandbox_id_errors() {
    let mut mgr = make_mgr();
    for cmd in [
        Command::new("sandbox_exec")
            .with_id("sb-deadbeef")
            .with_args(vec!["true".into()]),
        Command::new("sandbox_info").with_id("sb-deadbeef"),
        Command::new("sandbox_kill").with_id("sb-deadbeef"),
    ] {
        let name = cmd.command.clone();
        let resp = execute(&mut mgr, cmd).await;
        assert!(!resp.is_ok(), "{} should fail", name);
        assert!(
            resp.error.unwrap().contains("not found"),
            "{} should report not found",
            name
        );
    }
}

/// sandbox_list filters by tenant.
#[tokio::test]
async fn test_sandbox_list_tenant_filter() {
    let mut mgr = make_mgr();
    for tenant in ["research", "research", "other"] {
        let resp = execute(
            &mut mgr,
            Command::create("unused", "/fake/vmlinux")
                .with_command("sandbox_create")
                .with_tenant(tenant),
        )
        .await;
        assert!(resp.is_ok(), "create {}: {:?}", tenant, resp);
    }

    let resp = execute(&mut mgr, Command::new("sandbox_list")).await;
    assert_eq!(resp.data.unwrap()["count"], 3);

    let resp = execute(
        &mut mgr,
        Command::new("sandbox_list").with_tenant("research"),
    )
    .await;
    let data = resp.data.unwrap();
    assert_eq!(data["count"], 2);
    for item in data["sandboxes"].as_array().unwrap() {
        assert_eq!(item["tenant"], "research");
    }
}

/// sandbox_kill: kills live sessions of this sandbox (via the session_kill
/// path), removes the workdir in the guest, and drops the record. The
/// tenant VM keeps running.
#[tokio::test]
async fn test_sandbox_kill_sessions_workdir_record() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0)
        .with_exec_gate(gate.clone());
    let exec_log = adapter.exec_log();
    let kill_log = adapter.kill_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research"),
    )
    .await;
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();

    // A live background session inside this sandbox.
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["sleep".into(), "100".into()])
            .with_exec_mode("background"),
    )
    .await;
    let session_id = resp.data.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = execute(&mut mgr, Command::new("sandbox_kill").with_id(&id)).await;
    assert!(resp.is_ok(), "sandbox_kill should succeed: {:?}", resp);
    assert_eq!(resp.data.unwrap()["sessions_killed"], 1);

    // The live session was killed via the kill path...
    assert_eq!(
        kill_log.lock().unwrap().as_slice(),
        std::slice::from_ref(&session_id)
    );
    let resp = execute(
        &mut mgr,
        Command::new("session_status").with_session_id(&session_id),
    )
    .await;
    assert_eq!(resp.data.unwrap()["status"], "killed");

    // ...the workdir was rm -rf'd (unsandboxed, exact path)...
    {
        let log = exec_log.lock().unwrap();
        let rm = log.iter().find(|c| c.args[0] == "rm").expect("rm call");
        assert_eq!(rm.args, vec!["rm", "-rf", &format!("/workdir/{}", id)]);
        assert!(!rm.sandbox);
    }

    // ...the record is gone...
    let resp = execute(&mut mgr, Command::new("sandbox_info").with_id(&id)).await;
    assert!(!resp.is_ok());

    // ...and the tenant VM is still running.
    let resp = execute(&mut mgr, Command::new("info").with_name("tenant-research")).await;
    assert!(resp.is_ok(), "tenant VM must survive sandbox_kill");

    gate.notify_one();
}

/// tenant_destroy: destroys the tenant VM and drops all its sandbox records.
#[tokio::test]
async fn test_tenant_destroy_cascades() {
    let mut mgr = make_mgr();
    for _ in 0..2 {
        execute(
            &mut mgr,
            Command::create("unused", "/fake/vmlinux")
                .with_command("sandbox_create")
                .with_tenant("research"),
        )
        .await;
    }

    let resp = execute(
        &mut mgr,
        Command::new("tenant_destroy").with_tenant("research"),
    )
    .await;
    assert!(resp.is_ok(), "tenant_destroy should succeed: {:?}", resp);
    assert_eq!(resp.data.unwrap()["sandboxes_removed"], 2);

    // VM is gone, records are gone.
    let resp = execute(&mut mgr, Command::new("info").with_name("tenant-research")).await;
    assert!(!resp.is_ok());
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_list").with_tenant("research"),
    )
    .await;
    assert_eq!(resp.data.unwrap()["count"], 0);

    // Unknown tenant → honest error.
    let resp = execute(
        &mut mgr,
        Command::new("tenant_destroy").with_tenant("nosuch"),
    )
    .await;
    assert!(!resp.is_ok());
    assert!(resp.error.unwrap().contains("not found"));
}

/// sandbox_create stores the policy in the record; sandbox_info and
/// sandbox_list echo it back.
#[tokio::test]
async fn test_sandbox_create_stores_and_echoes_policy() {
    let mut mgr = make_mgr();
    let policy = make_policy(
        vec![
            Capability::File {
                path: PathPattern::Prefix("/opt/data".into()),
                access: FileAccess::Read,
            },
            Capability::File {
                path: PathPattern::Prefix("/output".into()),
                access: FileAccess::ReadWrite,
            },
            Capability::Network {
                endpoint: Endpoint {
                    host: "pypi.org".into(),
                    port: None,
                },
                direction: Direction::Outbound,
            },
        ],
        ResourceLimits {
            memory_mb: Some(512),
            procs: Some(20),
            ..Default::default()
        },
    );
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_policy(policy.clone()),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_create: {:?}", resp);
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();

    // sandbox_info echoes the stored policy (full roundtrip).
    let resp = execute(&mut mgr, Command::new("sandbox_info").with_id(&id)).await;
    assert!(resp.is_ok());
    let echoed = &resp.data.unwrap()["policy"];
    let back: SandboxPolicy = serde_json::from_value(echoed.clone()).unwrap();
    assert_eq!(back, policy);

    // sandbox_list echoes it too.
    let resp = execute(&mut mgr, Command::new("sandbox_list")).await;
    let data = resp.data.unwrap();
    let item = &data["sandboxes"][0];
    let back: SandboxPolicy = serde_json::from_value(item["policy"].clone()).unwrap();
    assert_eq!(back, policy);

    // A sandbox created without a policy echoes null.
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_create").with_tenant("research"),
    )
    .await;
    let id2 = resp.data.unwrap()["id"].as_str().unwrap().to_string();
    let resp = execute(&mut mgr, Command::new("sandbox_info").with_id(&id2)).await;
    assert!(resp.data.unwrap()["policy"].is_null());
}

/// G2: the two-layer invariant (policy-model §3.5) — a sandbox whose
/// requested limits fit within the tenant VM's physical quota creates fine.
/// The tenant VM cold-boots at 512 MB (build_spec default); 256 ≤ 512.
#[tokio::test]
async fn test_sandbox_create_limits_within_vm_quota_ok() {
    let mut mgr = make_mgr();
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_policy(make_policy(
                vec![],
                ResourceLimits {
                    memory_mb: Some(256),
                    ..Default::default()
                },
            )),
    )
    .await;
    assert!(resp.is_ok(), "limits within quota: {:?}", resp);
    assert_eq!(resp.data.unwrap()["vm"], "tenant-research");
}

/// G2: a sandbox requesting more memory than the tenant VM's physical
/// quota is rejected at create with the `validate_with_vm` message.
/// The tenant VM cold-boots at 512 MB; 1024 > 512.
#[tokio::test]
async fn test_sandbox_create_limits_exceeding_vm_quota_rejected() {
    let mut mgr = make_mgr();
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_policy(make_policy(
                vec![],
                ResourceLimits {
                    memory_mb: Some(1024),
                    ..Default::default()
                },
            )),
    )
    .await;
    assert!(!resp.is_ok(), "limits over VM quota must be rejected");
    assert!(
        resp.error.unwrap().contains("exceeds VM quota"),
        "validate_with_vm error message expected"
    );
}

/// G2: a tenant VM created with a small physical quota (256 MB) rejects a
/// sandbox requesting 512 MB.
#[tokio::test]
async fn test_sandbox_create_limits_vs_small_vm_rejected() {
    let mut mgr = make_mgr();
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_memory_mb(256)
            .with_policy(make_policy(
                vec![],
                ResourceLimits {
                    memory_mb: Some(512),
                    ..Default::default()
                },
            )),
    )
    .await;
    assert!(
        !resp.is_ok(),
        "512 MB limit on a 256 MB VM must be rejected"
    );
    assert!(
        resp.error.unwrap().contains("exceeds VM quota"),
        "validate_with_vm error message expected"
    );
}

/// sandbox_exec without a per-call policy inherits the stored one.
#[tokio::test]
async fn test_sandbox_exec_inherits_stored_policy() {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    let exec_log = adapter.exec_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

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
            .with_policy(stored.clone()),
    )
    .await;
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();
    exec_log.lock().unwrap().clear();

    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()]),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_exec: {:?}", resp);
    let log = exec_log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].sandbox);
    assert_eq!(
        log[0].policy.as_ref(),
        Some(&default_sandbox_policy().merged_with(&stored)),
        "exec must inherit the stored policy (base ∪ user)"
    );
}

/// A per-call policy on sandbox_exec overrides the stored one for that
/// call only.
#[tokio::test]
async fn test_sandbox_exec_policy_override_precedence() {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    let exec_log = adapter.exec_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    let stored = make_policy(
        vec![],
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
            .with_policy(stored.clone()),
    )
    .await;
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();
    exec_log.lock().unwrap().clear();

    let override_policy = make_policy(
        vec![],
        ResourceLimits {
            memory_mb: Some(512),
            procs: Some(10),
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
    assert_eq!(
        exec_log.lock().unwrap()[0].policy.as_ref(),
        Some(&default_sandbox_policy().merged_with(&override_policy)),
        "per-call policy must win over the stored one (base ∪ user)"
    );

    // Next call without a policy falls back to the stored one.
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()]),
    )
    .await;
    assert!(resp.is_ok());
    assert_eq!(
        exec_log.lock().unwrap()[1].policy.as_ref(),
        Some(&default_sandbox_policy().merged_with(&stored)),
        "override must not mutate the stored policy"
    );
}

/// sandbox_exec with sandbox:false plus a policy (per-call or stored) →
/// explicit error.
#[tokio::test]
async fn test_sandbox_exec_policy_requires_sandbox() {
    let mut mgr = make_mgr();
    let stored = make_policy(
        vec![],
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

    // Stored policy + sandbox:false → rejected.
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()])
            .with_sandbox(false),
    )
    .await;
    assert!(!resp.is_ok(), "stored policy + sandbox:false must fail");
    assert!(resp
        .error
        .unwrap()
        .contains("'policy' requires sandboxed exec"));

    // Per-call policy + sandbox:false on a policy-free sandbox → rejected.
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_create").with_tenant("research"),
    )
    .await;
    let id2 = resp.data.unwrap()["id"].as_str().unwrap().to_string();
    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id2)
            .with_args(vec!["echo".into(), "hi".into()])
            .with_sandbox(false)
            .with_policy(make_policy(vec![], ResourceLimits::default())),
    )
    .await;
    assert!(!resp.is_ok(), "per-call policy + sandbox:false must fail");
    assert!(resp
        .error
        .unwrap()
        .contains("'policy' requires sandboxed exec"));
}

/// sandbox_create with a Network capability that has an empty host →
/// explicit error (fail fast: a stored invalid policy would fail on every
/// later exec). The old "empty net_allow" footgun is gone — an empty
/// capability list is valid default-deny; the new validity rule is that
/// network endpoints must name a host.
#[tokio::test]
async fn test_sandbox_create_network_empty_host_rejected() {
    let mut mgr = make_mgr();
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
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
    assert!(resp
        .error
        .unwrap()
        .contains("network endpoint host must not be empty"));
}

/// sandbox_exec with a per-call Network capability with an empty host →
/// explicit error, even when the stored policy is valid.
#[tokio::test]
async fn test_sandbox_exec_network_empty_host_rejected() {
    let mut mgr = make_mgr();
    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_policy(make_policy(
                vec![Capability::Network {
                    endpoint: Endpoint {
                        host: "pypi.org".into(),
                        port: None,
                    },
                    direction: Direction::Outbound,
                }],
                ResourceLimits::default(),
            )),
    )
    .await;
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();

    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()])
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
    assert!(!resp.is_ok(), "empty network host override must fail");
    assert!(resp
        .error
        .unwrap()
        .contains("network endpoint host must not be empty"));
}

/// D2: sandbox_exec with no user policy (per-call or stored) → the engine
/// injects default_sandbox_policy(), so the guest always receives a
/// complete policy for sandboxed exec.
#[tokio::test]
async fn test_sandbox_exec_injects_default_policy() {
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("ok\n", "", 0);
    let exec_log = adapter.exec_log();
    let mut mgr = VmManager::new(Arc::new(adapter), "/tmp".into());

    let resp = execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research"),
    )
    .await;
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();
    exec_log.lock().unwrap().clear();

    let resp = execute(
        &mut mgr,
        Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["echo".into(), "hi".into()]),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_exec: {:?}", resp);
    let log = exec_log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].sandbox);
    assert_eq!(
        log[0].policy.as_ref(),
        Some(&terrarium_engine::policy::default_sandbox_policy()),
        "no user policy → engine injects the default"
    );
}

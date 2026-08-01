//! Tests for the default L2 backend: `GuestSandlockAdapter`.
//!
//! The backend wraps the engine's existing guest-sandlock exec path: it
//! must construct `ExecOpts{sandbox: true, policy: <bound ∪ override>}`
//! and forward them to `VmHandle::exec` unchanged in spirit — the recorded
//! `ExecCall` on the mock handle is the observable contract.

mod common;

use std::sync::Arc;

use adapter_traits::{
    AdapterError, Capability, DefaultAccess, ExecCommand, FileAccess, PathPattern, ResourceLimits,
    SandboxAdapter, SandboxHandle, SandboxPolicy, SandboxSpec, VmHandle, VmName,
};
use common::MockVmAdapter;
use terrarium_engine::sandbox_adapter::GuestSandlockAdapter;

/// Deny-by-default, version 1; reads /usr, read-writes /tmp.
fn bound_policy() -> SandboxPolicy {
    SandboxPolicy {
        capabilities: vec![
            Capability::File {
                path: PathPattern::Prefix("/usr".into()),
                access: FileAccess::Read,
            },
            Capability::File {
                path: PathPattern::Prefix("/tmp".into()),
                access: FileAccess::ReadWrite,
            },
        ],
        limits: ResourceLimits {
            memory_mb: Some(256),
            ..Default::default()
        },
        default: DefaultAccess::Deny,
        audit: Default::default(),
        version: 1,
    }
}

/// Adds an /opt read-write grant and a higher memory limit.
fn override_policy() -> SandboxPolicy {
    SandboxPolicy {
        capabilities: vec![Capability::File {
            path: PathPattern::Prefix("/opt".into()),
            access: FileAccess::ReadWrite,
        }],
        limits: ResourceLimits {
            memory_mb: Some(512),
            ..Default::default()
        },
        default: DefaultAccess::Deny,
        audit: Default::default(),
        version: 2,
    }
}

/// A bound session backed by a mock VM that records every exec.
async fn bind_session() -> (
    Box<dyn SandboxHandle>,
    Arc<std::sync::Mutex<Vec<common::ExecCall>>>,
) {
    let adapter = MockVmAdapter::new().with_exec("out\n", "", 0);
    let exec_log = adapter.exec_log();
    let vm: Arc<dyn VmHandle> = Arc::new(adapter.build_handle());
    let spec = SandboxSpec {
        name: VmName::new("session-vm").unwrap(),
        tools: vec![],
        limits: ResourceLimits::default(),
        env: Default::default(),
        policy: Some(bound_policy()),
    };
    let handle = GuestSandlockAdapter::new()
        .create(vm, &spec)
        .await
        .expect("create should succeed");
    (handle, exec_log)
}

#[tokio::test]
async fn create_binds_policy_and_runs_guestsandlock_exec() {
    let (handle, exec_log) = bind_session().await;

    let cmd = ExecCommand {
        args: vec!["echo".into(), "hi".into()],
        ..Default::default()
    };
    let result = handle.exec(&cmd).await.unwrap();
    assert_eq!(result.stdout, "out\n");

    let log = exec_log.lock().unwrap();
    let call = log.last().expect("one exec recorded");
    assert_eq!(call.args, vec!["echo", "hi"]);
    assert!(call.sandbox, "exec must run under sandlock confinement");
    assert_eq!(call.timeout_secs, 60);
    assert_eq!(call.exec_id, None, "blocking exec carries no exec_id");
    // No override → exactly the bound policy is passed through.
    let policy = call.policy.as_ref().expect("policy always present");
    assert!(policy.grants_path(std::path::Path::new("/usr/bin/ls"), FileAccess::Read));
    assert!(policy.grants_path(std::path::Path::new("/tmp/x"), FileAccess::ReadWrite));
    assert_eq!(policy.limits.memory_mb, Some(256));
}

#[tokio::test]
async fn exec_unions_policy_override_with_bound_policy() {
    let (handle, exec_log) = bind_session().await;

    let cmd = ExecCommand {
        args: vec!["echo".into()],
        policy_override: Some(override_policy()),
        ..Default::default()
    };
    handle.exec(&cmd).await.unwrap();

    let log = exec_log.lock().unwrap();
    let call = log.last().unwrap();
    let policy = call.policy.as_ref().expect("policy present");
    // Bound capabilities are preserved (base layer).
    assert!(policy.grants_path(std::path::Path::new("/usr/bin/ls"), FileAccess::Read));
    // Override capabilities are appended (union, not replace).
    assert!(policy.grants_path(std::path::Path::new("/opt/x"), FileAccess::ReadWrite));
    // Override limits win.
    assert_eq!(policy.limits.memory_mb, Some(512));
}

#[tokio::test]
async fn exec_passes_work_dir_through() {
    let (handle, exec_log) = bind_session().await;

    let cmd = ExecCommand {
        args: vec!["pwd".into()],
        work_dir: Some("/workdir/sb-abcd".into()),
        ..Default::default()
    };
    handle.exec(&cmd).await.unwrap();

    let log = exec_log.lock().unwrap();
    let call = log.last().unwrap();
    assert_eq!(call.work_dir.as_deref(), Some("/workdir/sb-abcd"));
    assert!(call.sandbox);
}

#[tokio::test]
async fn create_requires_an_effective_policy() {
    let adapter = MockVmAdapter::new();
    let vm: Arc<dyn VmHandle> = Arc::new(adapter.build_handle());
    let spec = SandboxSpec {
        name: VmName::new("session-vm").unwrap(),
        tools: vec![],
        limits: ResourceLimits::default(),
        env: Default::default(),
        // The engine injects its default before create; a bare spec here
        // must be rejected — the backend cannot confine without a policy.
        policy: None,
    };
    let err = match GuestSandlockAdapter::new().create(vm, &spec).await {
        Ok(_) => panic!("create without a policy must fail"),
        Err(e) => e,
    };
    assert!(matches!(err, AdapterError::InvalidArgument(_)), "{err}");
}

#[tokio::test]
async fn create_rejects_invalid_policy() {
    let adapter = MockVmAdapter::new();
    let vm: Arc<dyn VmHandle> = Arc::new(adapter.build_handle());
    // A relative file path fails SandboxPolicy::validate.
    let mut policy = bound_policy();
    policy.capabilities.push(Capability::File {
        path: PathPattern::Prefix("relative/path".into()),
        access: FileAccess::Read,
    });
    let spec = SandboxSpec {
        name: VmName::new("session-vm").unwrap(),
        tools: vec![],
        limits: ResourceLimits::default(),
        env: Default::default(),
        policy: Some(policy),
    };
    let err = match GuestSandlockAdapter::new().create(vm, &spec).await {
        Ok(_) => panic!("create with an invalid policy must fail"),
        Err(e) => e,
    };
    assert!(matches!(err, AdapterError::InvalidArgument(_)), "{err}");
}

#[tokio::test]
async fn setup_and_destroy_are_noops() {
    let (handle, _) = bind_session().await;
    handle.setup(&["python".into()]).await.unwrap();
    handle.destroy().await.unwrap();
}

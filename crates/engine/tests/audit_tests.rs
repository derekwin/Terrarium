//! Audit wiring tests (D-phase): the engine emits structured `audit.*`
//! tracing events per the effective policy's AuditSpec — `audit.exec` /
//! `audit.deny` when a sandboxed exec completes or is rejected, and
//! `audit.resource` on sandbox_create / VM resize. A recording subscriber
//! observes the events end-to-end through the command layer (no protocol
//! surface — the log stream is the audit channel).

mod common;

use std::sync::{Arc, Mutex};

use adapter_traits::{
    AuditSpec, Capability, DefaultAccess, FileAccess, PathPattern, SandboxPolicy,
};
use common::{MockSandboxAdapter, MockVmAdapter};
use terrarium_engine::commands::execute;
use terrarium_engine::manager::VmManager;
use terrarium_protocol::Command;
use tracing::field::Field;
use tracing::subscriber::with_default;
use tracing::{Event, Level, Metadata, Subscriber};

/// A recorded event: the level plus its (name, stringified value) fields.
type RecordedEvent = (Level, Vec<(String, String)>);

/// Test subscriber recording every event's field pairs (same harness as the
/// audit module's unit tests; duplicated here because test files are
/// separate crates with no shared code).
#[derive(Clone, Default)]
struct RecordingSubscriber {
    inner: Arc<RecordingInner>,
}

#[derive(Default)]
struct RecordingInner {
    events: Mutex<Vec<RecordedEvent>>,
}

impl Subscriber for RecordingSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Vec::new();
        event.record(&mut FieldCollector(&mut fields));
        self.inner
            .events
            .lock()
            .unwrap()
            .push((*event.metadata().level(), fields));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

struct FieldCollector<'a>(&'a mut Vec<(String, String)>);

impl<'a> tracing::field::Visit for FieldCollector<'a> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .push((field.name().to_string(), format!("{value:?}")));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
}

/// A policy that grants one path and carries the given audit spec.
fn policy_with_audit(audit: AuditSpec) -> SandboxPolicy {
    SandboxPolicy {
        capabilities: vec![Capability::File {
            path: PathPattern::Prefix("/opt".into()),
            access: FileAccess::Read,
        }],
        limits: Default::default(),
        default: DefaultAccess::Deny,
        audit,
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

/// Run an async engine command under a recording subscriber on the current
/// thread (blocking execs fire their audit events on the calling thread,
/// so the thread-local dispatcher captures them). Returns (result, events).
fn run_captured<T>(fut: impl std::future::Future<Output = T>) -> (T, Vec<Vec<(String, String)>>) {
    let sub = RecordingSubscriber::default();
    let result = with_default(sub.clone(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime")
            .block_on(fut)
    });
    let events = sub
        .inner
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|(_, fields)| fields.clone())
        .collect();
    (result, events)
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

async fn create_sandbox(mgr: &mut VmManager, tenant: &str, policy: &SandboxPolicy) -> String {
    let resp = execute(
        mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant(tenant)
            .with_policy(policy.clone()),
    )
    .await;
    assert!(resp.is_ok(), "sandbox_create failed: {:?}", resp);
    resp.data.unwrap()["id"].as_str().unwrap().to_string()
}

/// T2: a sandboxed blocking exec with `audit.exec` emits a structured
/// `audit.exec` event carrying the sandbox id, args, exit code and duration.
#[test]
fn sandbox_exec_emits_audit_exec_event() {
    let user = policy_with_audit(AuditSpec {
        exec: true,
        deny: false,
        resource: false,
    });
    let mut mgr = make_mgr(MockSandboxAdapter::new());
    let ((resp, id), events) = run_captured(async {
        let id = create_sandbox(&mut mgr, "research", &user).await;
        let resp = execute(
            &mut mgr,
            Command::create("unused", "/fake/vmlinux")
                .with_command("sandbox_exec")
                .with_id(&id)
                .with_args(vec!["echo".into(), "hi".into()])
                .with_sandbox(true)
                .with_policy(user),
        )
        .await;
        (resp, id)
    });
    assert!(resp.is_ok(), "sandbox_exec failed: {:?}", resp);

    let execs: Vec<_> = events
        .iter()
        .filter(|f| field(f, "audit") == Some("exec"))
        .collect();
    assert_eq!(execs.len(), 1, "one audit.exec event, got {events:?}");
    assert_eq!(field(execs[0], "sandbox_id"), Some(id.as_str()));
    assert_eq!(field(execs[0], "args"), Some("[\"echo\", \"hi\"]"));
    assert_eq!(field(execs[0], "exit_code"), Some("0"));
    assert!(field(execs[0], "duration_ms").is_some());
}

/// T2: with `audit.exec` off, the same exec emits no audit event.
#[test]
fn sandbox_exec_with_audit_off_emits_nothing() {
    let user = policy_with_audit(AuditSpec::default());
    let mut mgr = make_mgr(MockSandboxAdapter::new());
    let (resp, events) = run_captured(async {
        let id = create_sandbox(&mut mgr, "research", &user).await;
        execute(
            &mut mgr,
            Command::create("unused", "/fake/vmlinux")
                .with_command("sandbox_exec")
                .with_id(&id)
                .with_args(vec!["cat".into()])
                .with_sandbox(true)
                .with_policy(user),
        )
        .await
    });
    assert!(resp.is_ok(), "sandbox_exec failed: {:?}", resp);
    let audit_events: Vec<_> = events
        .iter()
        .filter(|f| field(f, "audit").is_some())
        .collect();
    assert!(
        audit_events.is_empty(),
        "audit flags off → no events, got {events:?}"
    );
}

/// T3: a sandboxed exec rejected by the guest sandlock (stderr contains
/// "denied") emits `audit.deny` when `audit.deny` is set.
#[test]
fn denied_sandbox_exec_emits_audit_deny_event() {
    let user = policy_with_audit(AuditSpec {
        exec: true,
        deny: true,
        resource: false,
    });
    let mut mgr = make_mgr(MockSandboxAdapter::new().with_exec("", "sandlock: access denied\n", 1));
    let ((resp, id), events) = run_captured(async {
        let id = create_sandbox(&mut mgr, "research", &user).await;
        let resp = execute(
            &mut mgr,
            Command::create("unused", "/fake/vmlinux")
                .with_command("sandbox_exec")
                .with_id(&id)
                .with_args(vec!["cat".into(), "/etc/passwd".into()])
                .with_sandbox(true)
                .with_policy(user),
        )
        .await;
        (resp, id)
    });
    assert!(resp.is_ok(), "sandbox_exec failed: {:?}", resp);

    let denies: Vec<_> = events
        .iter()
        .filter(|f| field(f, "audit") == Some("deny"))
        .collect();
    assert_eq!(denies.len(), 1, "one audit.deny event, got {events:?}");
    assert_eq!(field(denies[0], "sandbox_id"), Some(id.as_str()));
    assert!(
        field(denies[0], "reason").unwrap_or("").contains("denied"),
        "reason carries the sandlock rejection"
    );
}

/// T4: `sandbox_create` with `audit.resource` emits a resource-declaration
/// event carrying the declared limits.
#[test]
fn sandbox_create_emits_audit_resource_event() {
    let user = policy_with_audit(AuditSpec {
        exec: false,
        deny: false,
        resource: true,
    });
    let mut mgr = make_mgr(MockSandboxAdapter::new());

    let (resp, events) = run_captured(execute(
        &mut mgr,
        Command::create("unused", "/fake/vmlinux")
            .with_command("sandbox_create")
            .with_tenant("research")
            .with_policy(user),
    ));
    assert!(resp.is_ok(), "sandbox_create failed: {:?}", resp);
    let id = resp.data.unwrap()["id"].as_str().unwrap().to_string();

    let resources: Vec<_> = events
        .iter()
        .filter(|f| field(f, "audit") == Some("resource") && field(f, "kind") == Some("limits"))
        .collect();
    assert_eq!(
        resources.len(),
        1,
        "one limits resource event, got {events:?}"
    );
    assert_eq!(field(resources[0], "sandbox_id"), Some(id.as_str()));
    assert!(field(resources[0], "detail").is_some());
}

/// M3 regression: on the C3 handle path the backend executes
/// `bound ∪ per-call`, so the STORED policy's audit flags must survive a
/// per-call override that carries none. The audit events gate on the
/// actually-executed policy, not on the replace-chain
/// `default.merged_with(per_call.or(stored))` (which silently drops the
/// stored policy — including its audit flags — whenever an override is
/// present). Pre-fix: the executed exec had `audit.exec=true` but the
/// gating policy had `audit.exec=false` → no event was emitted.
#[test]
fn stored_audit_exec_survives_per_call_override_without_audit() {
    let stored = policy_with_audit(AuditSpec {
        exec: true,
        deny: false,
        resource: false,
    });
    let per_call = policy_with_audit(AuditSpec::default()); // all-false audit
    let sandbox = MockSandboxAdapter::new();
    let handle_log = sandbox.exec_log();
    let mut mgr = make_mgr(sandbox);
    let ((resp, id), events) = run_captured(async {
        let id = create_sandbox(&mut mgr, "research", &stored).await;
        let resp = execute(
            &mut mgr,
            Command::create("unused", "/fake/vmlinux")
                .with_command("sandbox_exec")
                .with_id(&id)
                .with_args(vec!["echo".into(), "hi".into()])
                .with_sandbox(true)
                .with_policy(per_call),
        )
        .await;
        (resp, id)
    });
    assert!(resp.is_ok(), "sandbox_exec failed: {:?}", resp);

    // The raw per-call override reached the bound handle (C3 plumbing)...
    let log = handle_log.lock().unwrap();
    assert_eq!(log.len(), 1, "one handle.exec call");
    assert!(
        log[0].policy_override.is_some(),
        "the per-call override must reach the handle"
    );

    // ...yet the stored audit.exec flag still gates an audit event.
    let execs: Vec<_> = events
        .iter()
        .filter(|f| field(f, "audit") == Some("exec"))
        .collect();
    assert_eq!(
        execs.len(),
        1,
        "stored audit.exec must survive a per-call override, got {events:?}"
    );
    assert_eq!(field(execs[0], "sandbox_id"), Some(id.as_str()));
}

/// M3 symmetric: a denied exec (stderr "denied") with the stored
/// `audit.deny` flag and a per-call override carrying no audit flags must
/// still emit `audit.deny` — the gating policy is the executed one
/// (`bound ∪ per_call`), not the replace-chain that drops the stored
/// policy.
#[test]
fn stored_audit_deny_survives_per_call_override_without_audit() {
    let stored = policy_with_audit(AuditSpec {
        exec: false,
        deny: true,
        resource: false,
    });
    let per_call = policy_with_audit(AuditSpec::default());
    let mut mgr = make_mgr(MockSandboxAdapter::new().with_exec("", "sandlock: access denied\n", 1));
    let ((resp, id), events) = run_captured(async {
        let id = create_sandbox(&mut mgr, "research", &stored).await;
        let resp = execute(
            &mut mgr,
            Command::create("unused", "/fake/vmlinux")
                .with_command("sandbox_exec")
                .with_id(&id)
                .with_args(vec!["cat".into(), "/etc/passwd".into()])
                .with_sandbox(true)
                .with_policy(per_call),
        )
        .await;
        (resp, id)
    });
    assert!(resp.is_ok(), "sandbox_exec failed: {:?}", resp);

    let denies: Vec<_> = events
        .iter()
        .filter(|f| field(f, "audit") == Some("deny"))
        .collect();
    assert_eq!(
        denies.len(),
        1,
        "stored audit.deny must survive a per-call override, got {events:?}"
    );
    assert_eq!(field(denies[0], "sandbox_id"), Some(id.as_str()));
}

/// T4: VM resize is a platform action with no per-sandbox policy — it is
/// always audited as `audit = "resource", kind = "vm_resize"`.
#[test]
fn vm_resize_always_emits_audit_resource_event() {
    let mut mgr = VmManager::new(
        Arc::new(MockVmAdapter::new().with_state("Running")),
        "/tmp".into(),
    );
    let resp = run_captured(execute(&mut mgr, Command::create("r-vm", "/fake/vmlinux"))).0;
    assert!(resp.is_ok(), "create failed: {:?}", resp);

    let (resp, events) = run_captured(execute(
        &mut mgr,
        Command::create("r-vm", "/fake/vmlinux")
            .with_command("resize")
            .with_cpus(4),
    ));
    assert!(resp.is_ok(), "resize failed: {:?}", resp);

    let resizes: Vec<_> = events
        .iter()
        .filter(|f| field(f, "audit") == Some("resource") && field(f, "kind") == Some("vm_resize"))
        .collect();
    assert_eq!(resizes.len(), 1, "one vm_resize event, got {events:?}");
    assert_eq!(field(resizes[0], "vm_name"), Some("r-vm"));
    assert_eq!(field(resizes[0], "cpus"), Some("4"));
}

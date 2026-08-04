//! Audit events (D-phase, R6; productized in P2) — structured tracing
//! output gated by the per-policy AuditSpec, plus a bounded in-engine
//! ring buffer so the daemon can answer `audit_list` queries without a
//! log aggregator.
//!
//! Event model (consumers aggregate from the log stream):
//! - `audit.exec` — a sandboxed exec completed (exit_code + duration).
//! - `audit.deny` — a sandboxed exec rejected by policy. The guest
//!   sandlock denial is a structured signal: guest-proxy rewrites the
//!   child exit code to `adapter_traits::SANDBOX_DENY_EXIT_CODE` on
//!   detection, and this module matches on that code — never on stderr
//!   text, which is carried only as the informative `reason`.
//! - `audit.resource` — resource declarations / adjustments (sandbox limits
//!   at create; VM resize as an always-on platform event).
use adapter_traits::{AdapterError, ExecResult, SandboxPolicy, SANDBOX_DENY_EXIT_CODE};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// One in-engine audit record — the same event the tracing stream emits,
/// kept for query (`audit_list`).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AuditRecord {
    pub ts_ms: u64,
    pub event: String,
    /// Audit subject: the engine sandbox id, or the VM name for
    /// VM-level platform events (e.g. resize).
    pub sandbox_id: String,
    pub args: Vec<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub reason: Option<String>,
    pub kind: Option<String>,
    pub detail: Option<String>,
}

/// Bounded ring buffer capacity (drop oldest when full).
const AUDIT_CAPACITY: usize = 10_000;

fn store() -> &'static Mutex<VecDeque<AuditRecord>> {
    static LOG: OnceLock<Mutex<VecDeque<AuditRecord>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(VecDeque::with_capacity(AUDIT_CAPACITY)))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn push(record: AuditRecord) {
    let mut log = store().lock().unwrap_or_else(|e| e.into_inner());
    if log.len() >= AUDIT_CAPACITY {
        log.pop_front();
    }
    log.push_back(record);
}

/// Query the ring buffer (newest first), optionally filtered.
pub(crate) fn audit_list(
    limit: usize,
    event: Option<&str>,
    sandbox_id: Option<&str>,
) -> Vec<AuditRecord> {
    let log = store().lock().unwrap_or_else(|e| e.into_inner());
    log.iter()
        .rev()
        .filter(|r| event.map_or(true, |e| r.event == e))
        .filter(|r| sandbox_id.map_or(true, |s| r.sandbox_id == s))
        .take(limit)
        .cloned()
        .collect()
}

/// Clear the ring buffer (tests / operator reset).
#[allow(dead_code)] // used by tests; operator reset reserved for the audit API
pub(crate) fn audit_clear() {
    store().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// `audit.exec` — a sandbox exec completed, with its exit code and
/// wall-clock duration. Gated by `policy.audit.exec`; `None` policy emits
/// nothing (no policy → nothing to gate on).
pub(crate) fn audit_exec(
    policy: Option<&SandboxPolicy>,
    sandbox: &str,
    args: &[String],
    exit_code: i32,
    duration_ms: u64,
) {
    if policy.map(|p| p.audit.exec).unwrap_or(false) {
        tracing::info!(audit = "exec", sandbox_id = sandbox, args = ?args, exit_code = exit_code, duration_ms = duration_ms, "sandbox exec audited");
        push(AuditRecord {
            ts_ms: now_ms(),
            event: "exec".into(),
            sandbox_id: sandbox.to_string(),
            args: args.to_vec(),
            exit_code: Some(exit_code),
            duration_ms: Some(duration_ms),
            reason: None,
            kind: None,
            detail: None,
        });
    }
}

/// `audit.deny` — a sandboxed exec rejected by policy. Gated by
/// `policy.audit.deny`; `None` policy emits nothing.
pub(crate) fn audit_deny(
    policy: Option<&SandboxPolicy>,
    sandbox: &str,
    args: &[String],
    reason: &str,
) {
    if policy.map(|p| p.audit.deny).unwrap_or(false) {
        tracing::warn!(audit = "deny", sandbox_id = sandbox, args = ?args, reason = reason, "sandbox exec denied by policy");
        push(AuditRecord {
            ts_ms: now_ms(),
            event: "deny".into(),
            sandbox_id: sandbox.to_string(),
            args: args.to_vec(),
            exit_code: Some(SANDBOX_DENY_EXIT_CODE),
            duration_ms: None,
            reason: Some(reason.to_string()),
            kind: None,
            detail: None,
        });
    }
}

/// `audit.resource` — a resource declaration or adjustment tied to a
/// sandbox. Gated by `policy.audit.resource`; `None` policy emits nothing.
pub(crate) fn audit_resource(
    policy: Option<&SandboxPolicy>,
    sandbox: &str,
    kind: &str,
    detail: &str,
) {
    if policy.map(|p| p.audit.resource).unwrap_or(false) {
        tracing::info!(
            audit = "resource",
            sandbox_id = sandbox,
            kind = kind,
            detail = detail,
            "sandbox resource audit"
        );
        push(AuditRecord {
            ts_ms: now_ms(),
            event: "resource".into(),
            sandbox_id: sandbox.to_string(),
            args: Vec::new(),
            exit_code: None,
            duration_ms: None,
            reason: None,
            kind: Some(kind.to_string()),
            detail: Some(detail.to_string()),
        });
    }
}

/// Record the audit events for one completed exec: `audit.exec` when the
/// exec completed with an exit code, plus `audit.deny` when it exited with
/// [`SANDBOX_DENY_EXIT_CODE`] — the structured guest sandlock deny signal
/// (guest-proxy rewrites the child exit code to it on detection; see the
/// constant's docs). The stderr text is carried as the deny `reason` —
/// informative, never a signal. An `AdapterError` is a transport failure,
/// not a policy denial, and emits nothing.
///
/// Shared by every exec path (blocking `run_exec`, the daemon's lock-free
/// prepared path, and background sessions) so the audit semantics stay
/// identical regardless of how the exec was served.
pub(crate) fn audit_exec_outcome(
    policy: Option<&SandboxPolicy>,
    sandbox_id: &str,
    args: &[String],
    result: &Result<ExecResult, AdapterError>,
    duration_ms: u64,
) {
    match result {
        Ok(r) => {
            audit_exec(policy, sandbox_id, args, r.exit_code, duration_ms);
            if r.exit_code == SANDBOX_DENY_EXIT_CODE {
                audit_deny(policy, sandbox_id, args, r.stderr.trim());
            }
        }
        Err(_) => {
            // A policy denial arrives as an Ok result with the reserved
            // deny exit code — never as an AdapterError.
        }
    }
}

/// `audit.resource` for a VM-level resize. There is no per-sandbox policy
/// at the VM layer, so this is a platform action and always audited (not
/// gated by an AuditSpec).
pub(crate) fn audit_vm_resize(vm: &str, cpus: Option<u32>, memory_bytes: Option<u64>) {
    tracing::info!(
        audit = "resource",
        kind = "vm_resize",
        vm_name = vm,
        cpus = cpus,
        memory_bytes = memory_bytes,
        "vm resize audited"
    );
    push(AuditRecord {
        ts_ms: now_ms(),
        event: "resource".into(),
        sandbox_id: vm.to_string(),
        args: Vec::new(),
        exit_code: None,
        duration_ms: None,
        reason: None,
        kind: Some("vm_resize".into()),
        detail: Some(format!("cpus={:?} memory_bytes={:?}", cpus, memory_bytes)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use adapter_traits::{AuditSpec, SANDBOX_DENY_EXIT_CODE};
    use std::sync::{Arc, Mutex};
    use tracing::field::Field;
    use tracing::subscriber::with_default;
    use tracing::{Event, Level, Metadata, Subscriber};

    /// A recorded event: the level plus its (name, stringified value) fields.
    type RecordedEvent = (Level, Vec<(String, String)>);

    /// Test subscriber that records every event's (level, field pairs).
    /// `Clone` shares the same recording buffer, so the test can hand a
    /// clone to `with_default` and still assert on the events afterwards.
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

    /// Collects recorded field values into (name, stringified) pairs.
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

    fn audited_policy() -> SandboxPolicy {
        SandboxPolicy {
            audit: AuditSpec {
                deny: true,
                exec: true,
                resource: true,
            },
            ..Default::default()
        }
    }

    fn unaudited_policy() -> SandboxPolicy {
        SandboxPolicy::default()
    }

    fn recorded(sub: &RecordingSubscriber) -> Vec<Vec<(String, String)>> {
        sub.inner
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|(_, fields)| fields.clone())
            .collect()
    }

    fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn audit_exec_gated_off_emits_nothing() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_exec(
                Some(&unaudited_policy()),
                "sb-x",
                &["echo".into(), "hi".into()],
                0,
                5,
            );
        });
        assert!(recorded(&sub).is_empty(), "audit=false must not emit");
    }

    #[test]
    fn audit_exec_with_no_policy_emits_nothing() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_exec(None, "sb-x", &["echo".into()], 0, 5);
        });
        assert!(recorded(&sub).is_empty(), "no policy must not emit");
    }

    #[test]
    fn audit_exec_gated_on_emits_expected_fields() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_exec(
                Some(&audited_policy()),
                "sb-x",
                &["echo".into(), "hi".into()],
                0,
                42,
            );
        });
        let events = recorded(&sub);
        assert_eq!(events.len(), 1, "exactly one event");
        let fields = &events[0];
        assert_eq!(field(fields, "audit"), Some("exec"));
        assert_eq!(field(fields, "sandbox_id"), Some("sb-x"));
        assert_eq!(field(fields, "args"), Some("[\"echo\", \"hi\"]"));
        assert_eq!(field(fields, "exit_code"), Some("0"));
        assert_eq!(field(fields, "duration_ms"), Some("42"));
    }

    #[test]
    fn audit_deny_gated_off_emits_nothing() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_deny(
                Some(&unaudited_policy()),
                "sb-x",
                &["cat".into()],
                "sandlock: denied",
            );
        });
        assert!(recorded(&sub).is_empty(), "audit.deny=false must not emit");
    }

    #[test]
    fn audit_deny_gated_on_emits_reason() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_deny(
                Some(&audited_policy()),
                "sb-x",
                &["cat".into()],
                "sandlock: denied",
            );
        });
        let events = recorded(&sub);
        assert_eq!(events.len(), 1);
        assert_eq!(field(&events[0], "audit"), Some("deny"));
        assert_eq!(field(&events[0], "sandbox_id"), Some("sb-x"));
        assert_eq!(field(&events[0], "reason"), Some("sandlock: denied"));
    }

    #[test]
    fn audit_resource_gated_off_emits_nothing() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_resource(Some(&unaudited_policy()), "sb-x", "limits", "limits: None");
        });
        assert!(
            recorded(&sub).is_empty(),
            "audit.resource=false must not emit"
        );
    }

    #[test]
    fn audit_resource_gated_on_emits_kind_and_detail() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_resource(
                Some(&audited_policy()),
                "sb-x",
                "limits",
                "ResourceLimits { memory_mb: Some(512) }",
            );
        });
        let events = recorded(&sub);
        assert_eq!(events.len(), 1);
        assert_eq!(field(&events[0], "audit"), Some("resource"));
        assert_eq!(field(&events[0], "kind"), Some("limits"));
        assert_eq!(
            field(&events[0], "detail"),
            Some("ResourceLimits { memory_mb: Some(512) }")
        );
    }

    #[test]
    fn audit_vm_resize_always_emits() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_vm_resize("tenant-x", Some(2), Some(1024 * 1024 * 1024));
        });
        let events = recorded(&sub);
        assert_eq!(events.len(), 1);
        assert_eq!(field(&events[0], "audit"), Some("resource"));
        assert_eq!(field(&events[0], "kind"), Some("vm_resize"));
        assert_eq!(field(&events[0], "vm_name"), Some("tenant-x"));
        assert_eq!(field(&events[0], "cpus"), Some("2"));
        assert_eq!(field(&events[0], "memory_bytes"), Some("1073741824"));
    }

    /// A sandlock denial as it now arrives (M4): the reserved deny exit
    /// code with a marker-free stderr — the structured signal is the code,
    /// the stderr text is only the informative reason.
    fn denied_result() -> Result<ExecResult, AdapterError> {
        Ok(ExecResult {
            stdout: String::new(),
            stderr: "sandlock: EACCES opening /etc/passwd".into(),
            exit_code: SANDBOX_DENY_EXIT_CODE,
        })
    }

    #[test]
    fn exec_outcome_emits_exec_and_deny_on_deny_exit_code() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_exec_outcome(
                Some(&audited_policy()),
                "sb-x",
                &["cat".into()],
                &denied_result(),
                7,
            );
        });
        let events = recorded(&sub);
        assert_eq!(events.len(), 2, "exec + deny events");
        let kinds: Vec<_> = events.iter().map(|f| field(f, "audit").unwrap()).collect();
        assert!(kinds.contains(&"exec"));
        assert!(kinds.contains(&"deny"));
        let deny = events
            .iter()
            .find(|f| field(f, "audit") == Some("deny"))
            .unwrap();
        assert_eq!(field(deny, "duration_ms"), None, "deny carries no duration");
        assert_eq!(
            field(deny, "reason"),
            Some("sandlock: EACCES opening /etc/passwd"),
            "the stderr text is the informative reason, not the signal"
        );
    }

    #[test]
    fn exec_outcome_nonzero_without_denied_text_emits_exec_only() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_exec_outcome(
                Some(&audited_policy()),
                "sb-x",
                &["false".into()],
                &Ok(ExecResult {
                    stdout: String::new(),
                    stderr: "command not found".into(),
                    exit_code: 127,
                }),
                3,
            );
        });
        let events = recorded(&sub);
        assert_eq!(events.len(), 1, "only exec, no deny");
        assert_eq!(field(&events[0], "audit"), Some("exec"));
        assert_eq!(field(&events[0], "exit_code"), Some("127"));
    }

    #[test]
    fn exec_outcome_engine_error_emits_nothing() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_exec_outcome(
                Some(&audited_policy()),
                "sb-x",
                &["cat".into()],
                &Err(AdapterError::internal("sandlock denied the exec")),
                0,
            );
        });
        assert!(
            recorded(&sub).is_empty(),
            "transport errors are not policy denials — no events"
        );
    }

    #[test]
    fn exec_outcome_gated_off_emits_nothing() {
        let sub = RecordingSubscriber::default();
        with_default(sub.clone(), || {
            audit_exec_outcome(
                Some(&unaudited_policy()),
                "sb-x",
                &["cat".into()],
                &denied_result(),
                7,
            );
        });
        assert!(recorded(&sub).is_empty(), "both flags false → nothing");
    }

    /// The in-engine ring buffer (P2 audit_list) records gated events and
    /// supports filtering; audit_clear resets it.
    #[test]
    fn audit_list_queries_the_ring_buffer() {
        audit_clear();
        let policy = audited_policy();
        audit_exec(Some(&policy), "sb-1", &["echo".into()], 0, 5);
        audit_deny(Some(&policy), "sb-1", &["cat".into()], "denied");

        let all = audit_list(100, None, Some("sb-1"));
        assert_eq!(all.len(), 2, "both records are stored");
        assert_eq!(all[0].event, "deny", "newest first");
        assert_eq!(all[0].reason.as_deref(), Some("denied"));
        assert_eq!(all[1].event, "exec");
        assert_eq!(all[1].exit_code, Some(0));
        assert_eq!(all[1].duration_ms, Some(5));

        let execs = audit_list(100, Some("exec"), Some("sb-1"));
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].event, "exec");

        let other = audit_list(100, None, Some("sb-other"));
        assert!(other.is_empty(), "sandbox filter applies");

        audit_clear();
        assert!(audit_list(100, None, Some("sb-1")).is_empty());
    }
}

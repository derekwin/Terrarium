//! Audit query (P2 — productized observability): read the engine's
//! bounded audit ring buffer.

use crate::audit;
use terrarium_protocol::{Command, Response};

/// {"command":"audit_list","limit"?:N,"event"?:"exec"|"deny"|"resource",
///  "id"?:<sandbox id or vm name>}
///
/// Returns `{audit: [...], count}` — newest first, filtered, bounded.
pub(crate) fn cmd_audit_list(cmd: &Command) -> Response {
    let limit = cmd.limit.unwrap_or(100).min(1000) as usize;
    let event = cmd.event.as_deref();
    let sandbox_id = cmd.id.as_deref();
    let records = audit::audit_list(limit, event, sandbox_id);
    let count = records.len();
    let audit: Vec<serde_json::Value> = records
        .into_iter()
        .map(|r| serde_json::to_value(r).unwrap_or_default())
        .collect();
    Response::ok(serde_json::json!({"audit": audit, "count": count}))
}

use crate::manager::VmManager;
use terrarium_protocol::{Command, Response};

pub(crate) fn cmd_session_status(mgr: &VmManager, cmd: Command) -> Response {
    let session_id = match cmd.session_id {
        Some(id) => id,
        None => return Response::err("Missing 'session_id' field"),
    };
    match mgr.session_status(&session_id) {
        Some(info) => Response::ok(serde_json::json!({
            "session_id": info.session_id,
            "vm_name": info.vm_name,
            "args": info.args,
            "status": info.status,
            "exit_code": info.exit_code,
            "stdout": info.stdout,
            "stderr": info.stderr,
            "sandbox": info.sandbox,
        })),
        None => Response::err(format!("Session '{}' not found", session_id)),
    }
}

/// Kill a background exec session: killpg the process group in the guest
/// (via a fresh vsock connection) and mark the session killed. Unknown or
/// non-running sessions and gone VMs fail loudly — never fake success.
pub(crate) async fn cmd_session_kill(mgr: &VmManager, cmd: Command) -> Response {
    let session_id = match cmd.session_id {
        Some(id) => id,
        None => return Response::err("Missing 'session_id' field"),
    };
    match mgr.session_kill(&session_id).await {
        Ok(()) => Response::ok(serde_json::json!({
            "session_id": session_id,
            "status": "killed",
        })),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) fn cmd_session_list(mgr: &VmManager) -> Response {
    let sessions = mgr.session_list();
    let items: Vec<_> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "session_id": s.session_id,
                "vm_name": s.vm_name,
                "status": s.status,
                "sandbox": s.sandbox,
            })
        })
        .collect();
    Response::ok(serde_json::json!({
        "sessions": items,
        "count": items.len(),
    }))
}

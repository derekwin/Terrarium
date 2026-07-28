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
        })),
        None => Response::err(format!("Session '{}' not found", session_id)),
    }
}

pub(crate) fn cmd_session_kill(mgr: &VmManager, cmd: Command) -> Response {
    let session_id = match cmd.session_id {
        Some(id) => id,
        None => return Response::err("Missing 'session_id' field"),
    };
    if mgr.session_kill(&session_id) {
        Response::ok(serde_json::json!({
            "session_id": session_id,
            "status": "killed",
        }))
    } else {
        Response::err(format!("Session '{}' not found", session_id))
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
            })
        })
        .collect();
    Response::ok(serde_json::json!({
        "sessions": items,
        "count": items.len(),
    }))
}

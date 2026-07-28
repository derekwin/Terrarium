use crate::manager::VmManager;
use terrarium_protocol::{Command, Response};

pub(crate) async fn cmd_snapshot(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    let vm = match mgr.get(&name) {
        Some(v) => v,
        None => return Response::err(format!("VM '{}' not found", name)),
    };
    let _path = cmd
        .snapshot_path
        .unwrap_or_else(|| format!("/tmp/terra-snap-{}.bin", name));

    match vm.snapshot().await {
        Ok(snap) => Response::ok(serde_json::json!({"snapshot_path": snap.path})),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) fn cmd_restore(_mgr: &mut VmManager, _cmd: Command) -> Response {
    Response::err("restore not implemented")
}

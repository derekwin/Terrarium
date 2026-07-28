use crate::manager::VmManager;
use terrarium_protocol::{Command, Response};

pub(crate) async fn cmd_snapshot(mgr: &VmManager, cmd: Command) -> Response {
    let (vm, name) = match super::get_vm(mgr, &cmd) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let _path = cmd
        .snapshot_path
        .unwrap_or_else(|| format!("{}/terra-snap-{}.bin", mgr.snapshot_dir(), name));

    match vm.snapshot().await {
        Ok(snap) => Response::ok(serde_json::json!({"snapshot_path": snap.path})),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) fn cmd_restore(_mgr: &mut VmManager, _cmd: Command) -> Response {
    Response::err("restore not implemented")
}

use crate::manager::VmManager;
use terrarium_protocol::{Command, Response};

pub(crate) async fn cmd_attach_fs(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name.as_deref() {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    if cmd.layers.is_empty() {
        return Response::err("Missing 'layers' field");
    }
    let fs = adapter_traits::FsSpec {
        layers: cmd.layers.clone(),
        upper: adapter_traits::UpperPolicy::Ephemeral,
    };
    match mgr.attach_fs(name, &fs).await {
        Ok(()) => Response::ok_msg(&format!("fs attached to VM '{}'", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

pub(crate) async fn cmd_detach_fs(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name.as_deref() {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    match mgr.detach_fs(name).await {
        Ok(()) => Response::ok_msg(&format!("fs detached from VM '{}'", name)),
        Err(e) => Response::err(e.to_string()),
    }
}

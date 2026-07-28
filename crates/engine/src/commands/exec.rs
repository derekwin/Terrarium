use crate::manager::VmManager;
use terrarium_protocol::{Command, Response};

pub(crate) async fn cmd_exec(mgr: &VmManager, cmd: Command) -> Response {
    let name = match cmd.name {
        Some(n) => n,
        None => return Response::err("Missing 'name' field"),
    };
    if cmd.args.is_empty() {
        return Response::err("Missing 'args' field");
    }
    let timeout = cmd.timeout_secs.unwrap_or(60).min(3600);
    match mgr.exec(&name, &cmd.args, timeout).await {
        Ok(r) => Response::ok(serde_json::json!({
            "stdout": r.stdout,
            "stderr": r.stderr,
            "exit_code": r.exit_code,
        })),
        Err(e) => Response::err(e.to_string()),
    }
}

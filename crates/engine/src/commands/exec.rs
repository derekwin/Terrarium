use super::{require_name, run_exec};
use crate::manager::VmManager;
use terrarium_protocol::{Command, Response};

pub(crate) async fn cmd_exec(mgr: &mut VmManager, cmd: Command) -> Response {
    let name = match require_name(&cmd) {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    run_exec(
        mgr,
        &name,
        &cmd.args,
        cmd.timeout_secs,
        false, // VM-scoped exec defaults to unsandboxed.
        cmd.sandbox,
        cmd.policy.clone(),
        None,
        cmd.exec_mode.clone(),
        None,
    )
    .await
}

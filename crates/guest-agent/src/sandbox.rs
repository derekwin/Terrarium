//! Sandbox execution: spawn and capture process output.
//!
//! Phase 1: plain process execution with stdout/stderr capture.
//! Namespace isolation (mount, UTS, IPC, net, pid) is deferred to Phase 2.

use std::process::{Command, Stdio};

/// Result of a command execution.
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Execute a command and capture its output.
pub fn exec_isolated(program: &str, args: &[String], work_dir: &str) -> Result<ExecResult, String> {
    let output = Command::new(program)
        .args(&args[1..])
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn failed: {}", e))?;

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

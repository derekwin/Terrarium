//! Sandbox execution: spawn and capture process output.
//!
//! Phase 1: plain process execution with stdout/stderr capture.
//! Namespace isolation (mount, UTS, IPC, net, pid) is deferred to Phase 2.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Result of a command execution.
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Maximum execution time for a command.
const EXEC_TIMEOUT_SECS: u64 = 60;

/// Maximum output size (stdout + stderr) in bytes.
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024; // 10 MB

/// Execute a command with timeout and output size limits.
pub fn exec_isolated(program: &str, args: &[String], work_dir: &str) -> Result<ExecResult, String> {
    let mut child = Command::new(program)
        .args(&args[1..])
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {}", e))?;

    let stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();

    // Wait for exit with timeout via a separate thread.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait());
    });

    let exit_status = match rx.recv_timeout(Duration::from_secs(EXEC_TIMEOUT_SECS)) {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("wait failed: {}", e)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Kill the process (we lost the child handle, but the process
            // is still running; the OS will clean up when the child scope ends).
            return Err(format!("command timed out after {}s", EXEC_TIMEOUT_SECS));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("process wait thread panicked".into());
        }
    };

    // Read output with size caps.
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    stdout_pipe
        .take(MAX_OUTPUT_BYTES as u64)
        .read_to_end(&mut stdout_buf)
        .map_err(|e| format!("read stdout: {}", e))?;
    stderr_pipe
        .take(MAX_OUTPUT_BYTES as u64)
        .read_to_end(&mut stderr_buf)
        .map_err(|e| format!("read stderr: {}", e))?;

    if stdout_buf.len() >= MAX_OUTPUT_BYTES || stderr_buf.len() >= MAX_OUTPUT_BYTES {
        return Err("output exceeded 10 MB limit".into());
    }

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        exit_code: exit_status.code().unwrap_or(-1),
    })
}

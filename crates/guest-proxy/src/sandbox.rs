//! Sandbox execution: spawn and capture process output.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

const EXEC_TIMEOUT_SECS: u64 = 60;
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

pub fn exec_isolated(program: &str, args: &[String], work_dir: &str) -> Result<ExecResult, String> {
    let mut child = Command::new(program)
        .args(&args[1..])
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {}", e))?;

    let pid = child.id();

    // Take pipes and spawn reader threads BEFORE waiting — avoids
    // pipe-buffer deadlock when child output exceeds 64KB.
    let stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();

    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        stdout_pipe
            .take(MAX_OUTPUT_BYTES as u64)
            .read_to_end(&mut buf)
            .ok();
        let _ = stdout_tx.send(buf);
    });
    thread::spawn(move || {
        let mut buf = Vec::new();
        stderr_pipe
            .take(MAX_OUTPUT_BYTES as u64)
            .read_to_end(&mut buf)
            .ok();
        let _ = stderr_tx.send(buf);
    });

    // Wait for child with timeout. Kill by pid on timeout.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait());
    });

    let exit_status = match rx.recv_timeout(Duration::from_secs(EXEC_TIMEOUT_SECS)) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("wait failed: {}", e)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).output();
            return Err(format!("command timed out after {}s", EXEC_TIMEOUT_SECS));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("process wait thread panicked".into());
        }
    };

    let stdout_buf = stdout_rx.recv().unwrap_or_default();
    let stderr_buf = stderr_rx.recv().unwrap_or_default();

    if stdout_buf.len() >= MAX_OUTPUT_BYTES || stderr_buf.len() >= MAX_OUTPUT_BYTES {
        return Err("output exceeded 10 MB limit".into());
    }

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        exit_code: exit_status.code().unwrap_or(-1),
    })
}

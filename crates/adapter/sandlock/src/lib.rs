//! Sandlock adapter — wraps Sandlock as a SandboxAdapter.
//!
//! Invokes `sandlock run` to confine processes with Landlock,
//! seccomp-bpf, seccomp notification, resource limits, HTTP ACL,
//! and COW filesystem — all without root.
//!
//! Requirements: `sandlock` binary in PATH.

use adapter_traits::{
    AdapterError, ExecCommand, ExecResult, SandboxAdapter, SandboxHandle, SandboxSpec, VmHandle,
    VmName,
};
use async_trait::async_trait;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[derive(Default)]
pub struct SandlockAdapter;

impl SandlockAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SandboxAdapter for SandlockAdapter {
    async fn create(
        &self,
        _vm: &dyn VmHandle,
        spec: &SandboxSpec,
    ) -> Result<Box<dyn SandboxHandle>, AdapterError> {
        Ok(Box::new(SandlockHandle {
            name: spec.name.clone(),
            tools: spec.tools.clone(),
            limits: spec.limits.clone(),
            env: spec.env.clone(),
        }))
    }
}

struct SandlockHandle {
    name: VmName,
    tools: Vec<String>,
    limits: adapter_traits::ResourceLimits,
    env: std::collections::HashMap<String, String>,
}

#[async_trait]
impl SandboxHandle for SandlockHandle {
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, AdapterError> {
        sandlock_run(cmd, &self)
    }

    async fn setup(&self, _tools: &[String]) -> Result<(), AdapterError> {
        // Sandlock has no persistent setup — it applies rules per-run
        Ok(())
    }

    async fn destroy(&self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// Build and invoke `sandlock run ...` with mapped flags.
fn sandlock_run(cmd: &ExecCommand, handle: &SandlockHandle) -> Result<ExecResult, AdapterError> {
    let mut args: Vec<String> = vec!["run".into()];

    // Filesystem: readable paths
    for tool in &handle.tools {
        match tool.as_str() {
            "python" | "python3" => {
                args.extend_from_slice(&[
                    "-r".into(),
                    "/usr".into(),
                    "-r".into(),
                    "/usr/local".into(),
                    "-r".into(),
                    "/lib".into(),
                    "-r".into(),
                    "/lib64".into(),
                    "-r".into(),
                    "/etc".into(),
                ]);
            }
            "node" | "nodejs" => {
                args.extend_from_slice(&[
                    "-r".into(),
                    "/usr".into(),
                    "-r".into(),
                    "/usr/local".into(),
                    "-r".into(),
                    "/lib".into(),
                    "-r".into(),
                    "/lib64".into(),
                ]);
            }
            _ => {}
        }
    }

    // Writable paths
    args.extend_from_slice(&["-w".into(), "/tmp".into()]);
    args.extend_from_slice(&["-w".into(), "/home/agent".into()]);

    // Resource limits
    if let Some(mb) = handle.limits.memory_mb {
        args.push("-m".into());
        args.push(format!("{}M", mb));
    }
    if let Some(_shares) = handle.limits.cpu_shares {
        // Map CPU shares (1024 = default) to process count limit.
        // Higher shares → more processes allowed.
        let max_procs = ((_shares as f64 / 1024.0) * 100.0).max(10.0) as u32;
        args.push("-P".into());
        args.push(max_procs.to_string());
    }

    // Environment: pass via process environment instead of --env argv
    // flags — argv is world-readable via /proc/<pid>/cmdline.
    // env_clear + whitelist replaces sandlock's --clean-env: the child
    // inherits exactly the requested variables and nothing else.
    // NOTE: assumes sandlock passes its own environment through to the
    // confined process; verify against the real sandlock binary.
    let mut process = Command::new("sandlock");
    if !handle.env.is_empty() {
        process.env_clear();
        // sandlock itself still needs PATH to resolve the target binary.
        if let Ok(path) = std::env::var("PATH") {
            process.env("PATH", path);
        }
        for (k, v) in &handle.env {
            process.env(k, v);
        }
    }

    // Command and its args
    args.push("--".into());
    for a in &cmd.args {
        args.push(a.clone());
    }

    tracing::info!(
        name = %handle.name,
        env_keys = ?handle.env.keys().collect::<Vec<_>>(),
        "sandlock run"
    );

    // Spawn with timeout (60s).
    let mut child = process
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| AdapterError::internal(format!("sandlock spawn: {}", e)))?;

    // Drain pipes concurrently BEFORE waiting — a child writing more than
    // the 64KB pipe buffer would otherwise block on write and never exit.
    let stdout_rx = spawn_pipe_reader(child.stdout.take().unwrap());
    let stderr_rx = spawn_pipe_reader(child.stderr.take().unwrap());

    // Poll try_wait so we keep the Child handle for a race-free kill
    // (no kill-by-pid, no detached wait thread, no zombie on timeout).
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                // SAFETY: pid is from Command::spawn(). Negative pid kills
                // the entire process group, preventing orphaned grandchildren.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Err(AdapterError::timeout(
                    "sandlock command timed out after 60s",
                ));
            }
            Err(e) => return Err(AdapterError::internal(format!("wait failed: {}", e))),
        }
    };

    let stdout_buf = stdout_rx.recv().unwrap_or_default();
    let stderr_buf = stderr_rx.recv().unwrap_or_default();

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        exit_code: exit_status.code().unwrap_or(-1),
    })
}

/// Max output captured per pipe for sandbox commands.
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

/// Spawn a thread that drains a pipe into a capped buffer.
fn spawn_pipe_reader(pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        pipe.take(MAX_OUTPUT_BYTES as u64)
            .read_to_end(&mut buf)
            .ok();
        let _ = tx.send(buf);
    });
    rx
}

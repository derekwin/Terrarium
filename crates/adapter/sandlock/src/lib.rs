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
        // Sandlock uses process count limits, not CPU shares
        args.push("-P".into());
        args.push("50".into());
    }

    // Environment variables.
    // NOTE: credentials in --env flags are visible in /proc/<pid>/cmdline.
    // For production, agents should read secrets from files or the tool
    // should support reading them from environment variables.
    let mut process = Command::new("sandlock");
    for (k, v) in &handle.env {
        args.push("--env".into());
        args.push(format!("{}={}", k, v));
        process.env(k, v);
    }

    // Clean environment by default
    if !handle.env.is_empty() {
        args.push("--clean-env".into());
    }

    // Command and its args
    args.push("--".into());
    for a in &cmd.args {
        args.push(a.clone());
    }

    tracing::info!(
        name = %handle.name,
        ?args,
        env_keys = ?handle.env.keys().collect::<Vec<_>>(),
        "sandlock run"
    );

    // Spawn with timeout (60s).
    let mut child = process
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AdapterError::internal(format!("sandlock spawn: {}", e)))?;

    // Take pipes before moving child into the wait thread.
    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait());
    });

    let exit_status = match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Ok(s)) => s,
        _ => {
            return Err(AdapterError::timeout(
                "sandlock command timed out after 60s",
            ))
        }
    };

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    stdout_pipe.read_to_end(&mut stdout_buf).ok();
    stderr_pipe.read_to_end(&mut stderr_buf).ok();

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        exit_code: exit_status.code().unwrap_or(-1),
    })
}

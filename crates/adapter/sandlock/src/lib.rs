//! Sandlock adapter — wraps Sandlock as a SandboxAdapter.
//!
//! Invokes `sandlock run` to confine processes with Landlock,
//! seccomp-bpf, seccomp notification, resource limits, HTTP ACL,
//! and COW filesystem — all without root.
//!
//! Requirements: `sandlock` binary in PATH.

use adapter_traits::{
    ExecCommand, ExecResult, SandboxAdapter, SandboxHandle, SandboxSpec, VmHandle, VmName,
};
use async_trait::async_trait;
use std::process::{Command, Stdio};

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
    ) -> Result<Box<dyn SandboxHandle>, String> {
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
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, String> {
        sandlock_run(cmd, &self)
    }

    async fn setup(&self, _tools: &[String]) -> Result<(), String> {
        // Sandlock has no persistent setup — it applies rules per-run
        Ok(())
    }

    async fn destroy(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Build and invoke `sandlock run ...` with mapped flags.
fn sandlock_run(cmd: &ExecCommand, handle: &SandlockHandle) -> Result<ExecResult, String> {
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

    // Environment
    for (k, v) in &handle.env {
        args.push("--env".into());
        args.push(format!("{}={}", k, v));
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

    tracing::info!(name = %handle.name, ?args, "sandlock run");

    let output = Command::new("sandlock")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("sandlock: {}", e))?;

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

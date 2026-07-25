//! OpenShell adapter — wraps OpenShell as a SandboxAdapter.
//!
//! Invokes `openshell sandbox create` to run agent code in container-sandboxed
//! environments with Landlock, seccomp-bpf, network namespace, and policy proxy.
//! Credentials are injected via `--env` flags.
//!
//! Requirements: OpenShell CLI and gateway running.

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
pub struct OpenshellAdapter;

impl OpenshellAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SandboxAdapter for OpenshellAdapter {
    async fn create(
        &self,
        _vm: &dyn VmHandle,
        spec: &SandboxSpec,
    ) -> Result<Box<dyn SandboxHandle>, AdapterError> {
        Ok(Box::new(OpenshellHandle {
            name: spec.name.clone(),
            tools: spec.tools.clone(),
            env: spec.env.clone(),
        }))
    }
}

struct OpenshellHandle {
    name: VmName,
    #[allow(dead_code)]
    tools: Vec<String>,
    env: std::collections::HashMap<String, String>,
}

#[async_trait]
impl SandboxHandle for OpenshellHandle {
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, AdapterError> {
        openshell_run(cmd, self.name.as_ref(), &self.env)
    }

    async fn setup(&self, _tools: &[String]) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn destroy(&self) -> Result<(), AdapterError> {
        // OpenShell sandboxes auto-destroy after command completion
        Ok(())
    }
}

fn openshell_run(
    cmd: &ExecCommand,
    name: &str,
    env: &std::collections::HashMap<String, String>,
) -> Result<ExecResult, AdapterError> {
    let mut args: Vec<String> = vec![
        "sandbox".into(),
        "create".into(),
        "--name".into(),
        name.into(),
    ];

    // Environment variables.
    // NOTE: credentials in --env flags are visible in /proc/<pid>/cmdline.
    // For production, agents should read secrets from files or the tool
    // should support reading them from environment variables.
    let mut process = Command::new("openshell");
    for (k, v) in env {
        args.push("--env".into());
        args.push(format!("{}={}", k, v));
        process.env(k, v);
    }

    // Command separator
    args.push("--".into());
    for a in &cmd.args {
        args.push(a.clone());
    }

    tracing::info!(
        name = %name,
        env_keys = ?env.keys().collect::<Vec<_>>(),
        "openshell run"
    );

    // Spawn with timeout (60s).
    let mut child = process
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AdapterError::internal(format!("openshell spawn: {}", e)))?;

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
                "openshell command timed out after 60s",
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

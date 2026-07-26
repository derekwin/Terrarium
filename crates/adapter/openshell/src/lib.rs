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
use std::os::unix::process::CommandExt;
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

    // Environment variables via --env flags.
    // KNOWN LIMITATION: openshell is a CLI front-end for a gateway, so
    // process-env inheritance cannot reach the sandboxed container — the
    // flags are the only injection channel. Credentials in argv are
    // visible via /proc/<pid>/cmdline for the (short) CLI lifetime.
    // Long-term fix: gateway-side secret references instead of literals.
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
        .process_group(0)
        .spawn()
        .map_err(|e| AdapterError::internal(format!("openshell spawn: {}", e)))?;

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
                    "openshell command timed out after 60s",
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

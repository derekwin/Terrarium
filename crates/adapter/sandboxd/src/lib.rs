//! sandboxd adapter — implements SandboxAdapter for our in-guest sandbox runtime.
//!
//! Communicates with the sandboxd daemon over Unix socket (guest-local in M2,
//! vsock-based in M3).

use adapter_traits::{
    ExecCommand, ExecResult, SandboxAdapter, SandboxHandle, SandboxSpec, VmHandle,
};
use async_trait::async_trait;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

#[derive(Default)]
pub struct SandboxdAdapter;

impl SandboxdAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SandboxAdapter for SandboxdAdapter {
    async fn create(
        &self,
        _vm: &dyn VmHandle,
        spec: &SandboxSpec,
    ) -> Result<Box<dyn SandboxHandle>, String> {
        // In M2, sandboxd communicates over guest-local Unix socket.
        // In M3, this will use vsock for host→guest communication.
        let handle = SandboxdHandle::new(&spec.name);
        if !spec.tools.is_empty() {
            handle.setup(&spec.tools).await?;
        }
        Ok(Box::new(handle))
    }
}

struct SandboxdHandle {
    #[allow(dead_code)]
    name: String,
}

impl SandboxdHandle {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }

    fn send_cmd(&self, json: &str) -> Result<String, String> {
        // Connect to sandboxd socket inside the guest.
        // In M2, this assumes sandboxd is on the same host (for testing).
        // In M3, this goes over vsock.
        let mut stream = UnixStream::connect("/tmp/sandboxd.sock")
            .map_err(|e| format!("connect sandboxd: {}", e))?;
        writeln!(stream, "{}", json).map_err(|e| format!("write: {}", e))?;
        stream.flush().map_err(|e| format!("flush: {}", e))?;

        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("read: {}", e))?;

        Ok(line.trim().to_string())
    }
}

#[async_trait]
impl SandboxHandle for SandboxdHandle {
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, String> {
        let mut req = serde_json::json!({
            "command": "exec",
            "args": cmd.args,
        });
        if let Some(ref wd) = cmd.work_dir {
            req["work_dir"] = serde_json::json!(wd);
        }
        if let Some(ref env) = cmd.env {
            req["env"] = serde_json::json!(env);
        }

        let resp = self.send_cmd(&req.to_string())?;
        let v: serde_json::Value =
            serde_json::from_str(&resp).map_err(|e| format!("parse: {}", e))?;

        if v["status"].as_str() != Some("ok") {
            return Err(v["message"].as_str().unwrap_or("unknown error").to_string());
        }

        let data = &v["data"];
        Ok(ExecResult {
            stdout: data["stdout"].as_str().unwrap_or("").to_string(),
            stderr: data["stderr"].as_str().unwrap_or("").to_string(),
            exit_code: data["exit_code"].as_i64().unwrap_or(-1) as i32,
        })
    }

    async fn setup(&self, tools: &[String]) -> Result<(), String> {
        for tool in tools {
            tracing::info!(tool = %tool, "sandboxd auto-setup on first exec");
        }
        Ok(())
    }

    async fn destroy(&self) -> Result<(), String> {
        // sandbox lifecycle is managed by the engine
        Ok(())
    }
}

//! Cloud Hypervisor adapter — self-contained VmAdapter implementation.
//!
//! Contains the CH HTTP API client (Unix socket) and VmAdapter trait impl.
//! No external CH SDK required — users install the official CH release binary.

pub mod api;
pub mod client;
mod config;
mod error;
mod fs;
mod process;

use crate::fs::{compose_fs, teardown_fs, FsStack};
use adapter_traits::{
    AdapterError, FsSpec, NetworkQos, Snapshot, VmAdapter, VmCapabilities, VmHandle, VmInfo,
    VmName, VmSpec,
};
use async_trait::async_trait;
use config::ChConfig;
use crate::process::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

pub use client::ChClient;
pub use error::ClientError;

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

pub struct ChAdapter {
    config: Arc<ChConfig>,
}

impl ChAdapter {
    pub fn new(ch_binary: impl Into<String>) -> Self {
        Self {
            config: Arc::new(ChConfig::from_env(ch_binary)),
        }
    }
}

#[async_trait]
impl VmAdapter for ChAdapter {
    fn capabilities(&self) -> VmCapabilities {
        VmCapabilities {
            cpu_resize: true,
            memory_resize: true,
            disk_resize: true,
            disk_add: true,
            snapshot: true,
            pause_resume: true,
            network_qos: true,
            virtio_fs: true,
        }
    }

    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>, AdapterError> {
        spec.validate().map_err(AdapterError::invalid_argument)?;
        ChVmHandle::spawn(spec, self)
            .await
            .map(|h| Box::new(h) as Box<dyn VmHandle>)
    }

    async fn restore(
        &self,
        _snapshot: &Snapshot,
        _spec: &VmSpec,
    ) -> Result<Box<dyn VmHandle>, AdapterError> {
        Err(AdapterError::not_supported(
            "CH restore not yet implemented via adapter",
        ))
    }
}

// ---------------------------------------------------------------------------
// VmHandle
// ---------------------------------------------------------------------------

struct ChVmHandle {
    name: VmName,
    child: std::process::Child,
    client: ChClient,
    fs: std::sync::Mutex<Option<FsStack>>,
    /// Device id of a hot-plugged fs device (needed for remove-device).
    fs_device_id: std::sync::Mutex<Option<String>>,
    /// Fs composition config cloned from the adapter (for hot-plug).
    config: Arc<ChConfig>,
}

impl ChVmHandle {
    async fn spawn(spec: &VmSpec, adapter: &ChAdapter) -> Result<Self, AdapterError> {
        let name = spec.name.clone();
        let socket = format!("/tmp/terra-{}.sock", name);
        let _ = std::fs::remove_file(&socket);

        // Compose the layered rootfs first — CH needs the virtiofsd
        // socket at boot.
        let fs = match spec.fs {
            Some(ref fs_spec) => Some(compose_fs(fs_spec, name.as_ref(), &adapter.config).await?),
            None => None,
        };
        let fs_socket = fs.as_ref().map(|f| f.socket.as_str());

        let vsock = format!("/tmp/terra-{}-vsock.sock", name);
        let _ = std::fs::remove_file(&vsock);

        // Networking: NAT bridge + per-VM tap (privileged; clear error).
        let tap = if spec.net {
            terrarium_network::ensure_nat_bridge(
                terrarium_network::DEFAULT_BRIDGE,
                terrarium_network::DEFAULT_GATEWAY,
                terrarium_network::DEFAULT_PREFIX,
            )
            .map_err(AdapterError::internal)?;
            let tap = format!("terra-{}", tap_name(name.as_ref()));
            terrarium_network::ensure_tap(&tap, terrarium_network::DEFAULT_BRIDGE)
                .map_err(AdapterError::internal)?;
            Some(tap)
        } else {
            None
        };

        let args = ch_args(spec, &socket, fs_socket, &vsock, tap.as_deref());

        tracing::info!(name = %name, socket = %socket, layered = fs.is_some(), "Spawning CH VM");

        let log_dir = format!("{}/logs", adapter.config.fs_root);
        let mut child = spawn_ch(&args, &adapter.config.ch_binary, &log_dir, name.as_ref())?;

        if let Err(e) = wait_for_socket(&socket, Duration::from_secs(15)).await {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }

        let client = ChClient::new(&socket).with_timeout(Duration::from_secs(5));
        tracing::info!(name = %name, "CH VM ready");

        Ok(Self {
            name,
            child,
            client,
            fs: std::sync::Mutex::new(fs),
            fs_device_id: std::sync::Mutex::new(None),
            config: adapter.config.clone(),
        })
    }
}

#[async_trait]
impl VmHandle for ChVmHandle {
    async fn info(&self) -> Result<VmInfo, AdapterError> {
        let details = retry_get_info(&self.client).await?;
        Ok(VmInfo {
            state: details.state,
            cpus: details
                .config
                .as_ref()
                .and_then(|c| c.cpus.as_ref())
                .map(|c| c.boot),
            memory_mb: details.memory_actual_size.map(|s| s / 1024 / 1024),
        })
    }

    async fn resize(&self, cpu: Option<u32>, memory: Option<u64>) -> Result<(), AdapterError> {
        self.client
            .vm_resize(cpu.map(|c| c as u8), memory)
            .await
            .map_err(|e| AdapterError::internal(format!("vm.resize: {}", e)))
    }

    async fn resize_disk(&self, disk_id: &str, size: u64) -> Result<(), AdapterError> {
        self.client
            .vm_resize_disk(disk_id, size)
            .await
            .map_err(|e| AdapterError::internal(format!("vm.resize-disk: {}", e)))
    }

    async fn add_disk(&self, path: &str, _disk_id: &str) -> Result<(), AdapterError> {
        self.client
            .vm_add_disk(path)
            .await
            .map_err(|e| AdapterError::internal(format!("vm.add-disk: {}", e)))
    }

    async fn exec(
        &self,
        args: &[String],
        timeout_secs: u64,
    ) -> Result<adapter_traits::ExecResult, AdapterError> {
        let resp = self
            .guest_cmd(&serde_json::json!({
                "command": "exec", "args": args, "timeout_secs": timeout_secs,
            }))
            .await?;
        if resp["status"].as_str() != Some("ok") {
            return Err(AdapterError::internal(format!(
                "guest exec failed: {}",
                resp["message"].as_str().unwrap_or("unknown")
            )));
        }
        let d = &resp["data"];
        Ok(adapter_traits::ExecResult {
            stdout: d["stdout"].as_str().unwrap_or_default().to_string(),
            stderr: d["stderr"].as_str().unwrap_or_default().to_string(),
            exit_code: d["exit_code"].as_i64().unwrap_or(-1) as i32,
        })
    }

    async fn attach_fs(&self, fs_spec: &FsSpec) -> Result<(), AdapterError> {
        if self.fs.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            return Err(AdapterError::internal(
                "an fs stack is already attached to this VM".to_string(),
            ));
        }
        // 1) compose layers + start virtiofsd on the host
        let stack = compose_fs(fs_spec, self.name.as_ref(), &self.config).await?;
        // 2) hot-plug the virtiofs device
        let device_id = self
            .client
            .vm_add_fs("rootfs", &stack.socket)
            .await
            .map_err(|e| AdapterError::internal(format!("vm.add-fs: {}", e)))?;
        // 3) mount inside the guest via guest-proxy vsock. The guest
        // agent may still be booting — retry briefly.
        // Retry while either the agent is unreachable or the device is
        // not yet enumerated (mount returns "Invalid argument" early).
        let mut resp = serde_json::Value::Null;
        for attempt in 0..20 {
            let err = match self
                .guest_cmd(&serde_json::json!({
                    "command": "mount", "tag": "rootfs", "target": "/workdir",
                }))
                .await
            {
                Ok(r) if r["status"].as_str() == Some("ok") => {
                    resp = r;
                    break;
                }
                Ok(r) => r["message"].as_str().unwrap_or("unknown").to_string(),
                Err(e) => e.to_string(),
            };
            if attempt == 19 {
                return Err(AdapterError::internal(format!(
                    "guest mount failed after retries: {}",
                    err
                )));
            }
            sleep(Duration::from_millis(500)).await;
        }
        debug_assert!(resp["status"].as_str() == Some("ok"));
        *self.fs_device_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(device_id);
        *self.fs.lock().unwrap_or_else(|e| e.into_inner()) = Some(stack);
        tracing::info!(name = %self.name, "fs attached (hot-plug)");
        Ok(())
    }

    async fn detach_fs(&self) -> Result<(), AdapterError> {
        // 1) best-effort guest umount
        let _ = self
            .guest_cmd(&serde_json::json!({"command": "umount", "target": "/workdir"}))
            .await;
        // 2) remove the device (take the lock guard before any await)
        let device_id = self
            .fs_device_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(id) = device_id {
            self.client
                .vm_remove_disk(&id)
                .await
                .map_err(|e| AdapterError::internal(format!("vm.remove-device: {}", e)))?;
        }
        // 3) tear down the host-side stack
        let stack = self.fs.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut fs) = stack {
            teardown_fs(&mut fs);
        }
        tracing::info!(name = %self.name, "fs detached");
        Ok(())
    }

    async fn set_network_qos(&self, qos: &NetworkQos) -> Result<(), AdapterError> {
        let tap = format!("tap-{}", self.name);
        terrarium_network::apply_tc_qos(&tap, qos).map_err(AdapterError::internal)
    }

    async fn pause(&self) -> Result<(), AdapterError> {
        self.client
            .vm_pause()
            .await
            .map_err(|e| AdapterError::internal(format!("vm.pause: {}", e)))
    }

    async fn resume(&self) -> Result<(), AdapterError> {
        self.client
            .vm_resume()
            .await
            .map_err(|e| AdapterError::internal(format!("vm.resume: {}", e)))
    }

    async fn snapshot(&self) -> Result<Snapshot, AdapterError> {
        let path = format!("/tmp/terra-snap-{}.bin", self.name);
        self.client
            .vm_snapshot(&path)
            .await
            .map_err(|e| AdapterError::internal(format!("vm.snapshot: {}", e)))?;
        Ok(Snapshot { path })
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        self.client
            .vm_shutdown()
            .await
            .map_err(|e| AdapterError::internal(format!("vm.shutdown: {}", e)))
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl ChVmHandle {
    /// Send one JSON command to guest-proxy over the CH vhost-vsock
    /// socket (text handshake "CONNECT <port>", then line-JSON).
    async fn guest_cmd(&self, cmd: &serde_json::Value) -> Result<serde_json::Value, AdapterError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let path = format!("/tmp/terra-{}-vsock.sock", self.name);
        let stream = tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| format!("connect guest vsock: {}", e))?;
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(b"CONNECT 1024\n")
            .await
            .map_err(|e| format!("vsock CONNECT: {}", e))?;
        let mut lines = BufReader::new(reader).lines();
        let handshake = lines
            .next_line()
            .await
            .map_err(|e| format!("vsock handshake: {}", e))?
            .unwrap_or_default();
        if !handshake.starts_with("OK") {
            return Err(format!("vsock handshake rejected: {}", handshake).into());
        }
        let mut payload = serde_json::to_string(cmd)
            .map_err(|e| AdapterError::internal(format!("serialize: {}", e)))?;
        payload.push('\n');
        writer
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| format!("vsock write: {}", e))?;
        let resp = lines
            .next_line()
            .await
            .map_err(|e| format!("vsock read: {}", e))?
            .unwrap_or_default();
        serde_json::from_str(&resp).map_err(|e| {
            AdapterError::internal(format!("guest-proxy response parse: {} ({})", e, resp))
        })
    }
}

impl Drop for ChVmHandle {
    fn drop(&mut self) {
        // Can't call async methods in Drop. Best-effort kill.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let socket = format!("/tmp/terra-{}.sock", self.name);
        let _ = std::fs::remove_file(&socket);
        // CH creates a lock file next to the API socket — remove it too,
        // otherwise stale .sock.lock files accumulate across VM lifecycles.
        let _ = std::fs::remove_file(format!("{}.lock", socket));
        if let Some(mut fs) = self.fs.lock().unwrap_or_else(|e| e.into_inner()).take() {
            // Killing the supervisor tears down the whole namespace:
            // virtiofsd dies and the overlayfs mount evaporates with it.
            teardown_fs(&mut fs);
        }
        // Remove the per-VM tap if networking was enabled (best-effort).
        let tap = format!("terra-{}", tap_name(self.name.as_ref()));
        let _ = terrarium_network::remove_tap(&tap);
    }
}

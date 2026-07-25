//! Firecracker adapter — implements VmAdapter for Firecracker.
//!
//! Self-contained: includes Firecracker HTTP API client + VmAdapter impl.
//! Spawns `firecracker` process with API socket, configures via REST API.
//!
//! Requirements: `firecracker` binary in PATH.

use adapter_traits::{
    NetworkQos, Snapshot, VmAdapter, VmCapabilities, VmHandle, VmInfo, VmName, VmSpec,
};
use async_trait::async_trait;
use overlay::{OverlaySpec, RawDiskManager};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct FirecrackerAdapter;

impl FirecrackerAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl VmAdapter for FirecrackerAdapter {
    fn capabilities(&self) -> VmCapabilities {
        VmCapabilities {
            cpu_resize: false,
            memory_resize: false,
            disk_resize: false,
            disk_add: false,
            snapshot: true,
            pause_resume: true,
            network_qos: false,
            qcow2: false,
        }
    }

    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>, String> {
        FcVmHandle::spawn(spec).map(|h| Box::new(h) as Box<dyn VmHandle>)
    }

    async fn restore(&self, _snap: &Snapshot, _spec: &VmSpec) -> Result<Box<dyn VmHandle>, String> {
        Err("Firecracker restore not yet implemented".into())
    }
}

// ── VmHandle ──────────────────────────────────────────────────────────

struct FcVmHandle {
    name: VmName,
    child: Child,
    socket: String,
}

impl FcVmHandle {
    fn spawn(spec: &VmSpec) -> Result<Self, String> {
        let name = spec.name.clone();
        let socket = format!("/tmp/fc-{}.sock", name);
        let _ = std::fs::remove_file(&socket);

        let mut child = Command::new("firecracker")
            .args(["--api-sock", &socket])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn firecracker: {}", e))?;

        // Wait for socket
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if std::path::Path::new(&socket).exists() {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("socket timeout".into());
            }
            thread::sleep(Duration::from_millis(100));
        }

        // Configure VM via HTTP API
        let client = FcClient::new(&socket);
        client.put(
            "/machine-config",
            &serde_json::json!({
                "vcpu_count": spec.boot_vcpus, "mem_size_mib": spec.memory_mb,
            }),
        )?;
        client.put(
            "/boot-source",
            &serde_json::json!({
                "kernel_image_path": spec.kernel,
                "boot_args": spec.cmdline.as_deref().unwrap_or("console=ttyS0"),
            }),
        )?;
        if let Some(ref initramfs) = spec.initramfs {
            client.put(
                "/boot-source",
                &serde_json::json!({
                    "kernel_image_path": spec.kernel,
                    "initrd_path": initramfs,
                }),
            )?;
        }
        // Attach root disk (convert qcow2→raw if needed)
        if let Some(ref root) = spec.base_disk {
            let ospec = OverlaySpec::new(name.to_string(), root)
                .disk_size_gb(spec.disk_size_gb)
                .state_dir("/tmp/terra-disks/vms");
            let disk_path =
                RawDiskManager::create_or_reuse(&ospec).map_err(|e| format!("raw disk: {}", e))?;
            client.put(
                "/drives/rootfs",
                &serde_json::json!({
                    "drive_id": "rootfs", "path_on_host": disk_path,
                    "is_root_device": true, "is_read_only": false,
                }),
            )?;
        }
        // Start VM
        client.put(
            "/actions",
            &serde_json::json!({"action_type": "InstanceStart"}),
        )?;

        tracing::info!(name = %name, "Firecracker VM started");
        Ok(Self {
            name,
            child,
            socket,
        })
    }

    fn fc_socket(&self) -> &str {
        &self.socket
    }
}

#[async_trait]
impl VmHandle for FcVmHandle {
    async fn info(&self) -> Result<VmInfo, String> {
        let client = FcClient::new(self.fc_socket());
        let resp = client.get("/")?;
        Ok(VmInfo {
            state: resp["state"].as_str().unwrap_or("Running").into(),
            cpus: resp["vcpu_count"].as_u64().map(|c| c as u8),
            memory_mb: resp["mem_size_mib"].as_u64(),
        })
    }

    async fn resize(&self, _cpu: Option<u32>, _mem: Option<u64>) -> Result<(), String> {
        Err("Firecracker does not support live CPU/memory resize".into())
    }

    async fn resize_disk(&self, _id: &str, _size: u64) -> Result<(), String> {
        Err("Firecracker does not support online disk resize".into())
    }

    async fn add_disk(&self, _path: &str, _id: &str) -> Result<(), String> {
        Err("Firecracker does not support hot-adding disks".into())
    }

    async fn pause(&self) -> Result<(), String> {
        let client = FcClient::new(self.fc_socket());
        client.patch("/vm", &serde_json::json!({"state": "Paused"}))
    }

    async fn resume(&self) -> Result<(), String> {
        let client = FcClient::new(self.fc_socket());
        client.patch("/vm", &serde_json::json!({"state": "Resumed"}))
    }

    async fn snapshot(&self) -> Result<Snapshot, String> {
        let vm_path = format!("/tmp/fc-snap-{}.bin", self.name);
        let mem_path = format!("/tmp/fc-snap-{}.mem", self.name);
        let client = FcClient::new(self.fc_socket());
        client.put(
            "/snapshot/create",
            &serde_json::json!({
                "snapshot_path": vm_path,
                "mem_file_path": mem_path,
                "snapshot_type": "Full",
            }),
        )?;
        Ok(Snapshot { path: vm_path })
    }

    async fn set_network_qos(&self, _qos: &NetworkQos) -> Result<(), String> {
        Err("Firecracker does not support per-VM network QoS".into())
    }

    async fn shutdown(&self) -> Result<(), String> {
        let client = FcClient::new(self.fc_socket());
        let _ = client.put(
            "/actions",
            &serde_json::json!({"action_type": "SendCtrlAltDel"}),
        );
        Ok(())
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for FcVmHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

// ── Minimal Firecracker HTTP client ───────────────────────────────────

struct FcClient {
    socket: String,
    timeout: Duration,
}

impl FcClient {
    fn new(socket: &str) -> Self {
        Self {
            socket: socket.into(),
            timeout: Duration::from_secs(5),
        }
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
        let mut stream =
            UnixStream::connect(&self.socket).map_err(|e| format!("connect: {}", e))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|e| format!("set timeout: {}", e))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|e| format!("set timeout: {}", e))?;

        let body_str = body.unwrap_or("");
        let req = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            method, path, body_str.len(), body_str
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("write: {}", e))?;
        stream.flush().map_err(|e| format!("flush: {}", e))?;

        let mut reader = BufReader::new(&stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| format!("read: {}", e))?;
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("invalid status line: {}", status_line.trim()))?;

        // Read Content-Length header
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|e| format!("read: {}", e))?;
            if line.trim().is_empty() {
                break;
            }
            if let Some(val) = line.to_lowercase().strip_prefix("content-length:") {
                content_length = val.trim().parse().unwrap_or(0);
            }
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader
                .read_exact(&mut body)
                .map_err(|e| format!("read body: {}", e))?;
        }

        if status >= 400 {
            return Err(format!(
                "HTTP {}: {}",
                status,
                String::from_utf8_lossy(&body)
            ));
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    fn put(&self, path: &str, body: &serde_json::Value) -> Result<(), String> {
        self.request("PUT", path, Some(&body.to_string()))?;
        Ok(())
    }

    fn patch(&self, path: &str, body: &serde_json::Value) -> Result<(), String> {
        self.request("PATCH", path, Some(&body.to_string()))?;
        Ok(())
    }

    fn get(&self, path: &str) -> Result<serde_json::Value, String> {
        let raw = self.request("GET", path, None)?;
        serde_json::from_str(&raw).map_err(|e| format!("parse: {}", e))
    }
}

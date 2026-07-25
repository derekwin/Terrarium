//! Cloud Hypervisor adapter — self-contained VmAdapter implementation.
//!
//! Contains the CH HTTP API client (Unix socket) and VmAdapter trait impl.
//! No external CH SDK required — users install the official CH release binary.

pub mod api;
pub mod client;
mod error;

use adapter_traits::{
    AdapterError, NetworkQos, Snapshot, VmAdapter, VmCapabilities, VmHandle, VmInfo, VmName, VmSpec,
};
use async_trait::async_trait;
use overlay::{OverlayManager, OverlaySpec};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::{sleep, Instant};

pub use client::ChClient;
pub use error::ClientError;

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

pub struct ChAdapter {
    ch_binary: String,
}

impl ChAdapter {
    pub fn new(ch_binary: impl Into<String>) -> Self {
        Self {
            ch_binary: ch_binary.into(),
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
            qcow2: true,
        }
    }

    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>, AdapterError> {
        spec.validate().map_err(AdapterError::invalid_argument)?;
        ChVmHandle::spawn(spec, &self.ch_binary)
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
}

impl ChVmHandle {
    async fn spawn(spec: &VmSpec, ch_binary: &str) -> Result<Self, AdapterError> {
        let name = spec.name.clone();
        let socket = format!("/tmp/terra-{}.sock", name);
        let _ = std::fs::remove_file(&socket);

        let mut args = ch_args(spec, &socket);

        if let Some(ref base) = spec.base_disk {
            let overlay_spec =
                OverlaySpec::new(name.to_string(), base).disk_size_gb(spec.disk_size_gb);
            let overlay = OverlayManager::create_or_reuse(&overlay_spec)?;
            args.push("--disk".to_string());
            args.push(format!("path={}", overlay));
        }

        tracing::info!(name = %name, socket = %socket, "Spawning CH VM");

        let mut child = Command::new(ch_binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn CH: {}", e))?;

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if std::path::Path::new(&socket).exists() {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("socket timeout for {}", name).into());
            }
            sleep(Duration::from_millis(100)).await;
        }

        let client = ChClient::new(&socket).with_timeout(Duration::from_secs(5));
        tracing::info!(name = %name, "CH VM ready");

        Ok(Self {
            name,
            child,
            client,
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

impl Drop for ChVmHandle {
    fn drop(&mut self) {
        // Can't call async methods in Drop. Best-effort kill.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let socket = format!("/tmp/terra-{}.sock", self.name);
        let _ = std::fs::remove_file(&socket);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ch_args(spec: &VmSpec, socket: &str) -> Vec<String> {
    let mut args = vec![
        "--api-socket".into(),
        socket.into(),
        "--kernel".into(),
        spec.kernel.clone(),
        "--cpus".into(),
        format!(
            "boot={},max={}",
            spec.boot_vcpus,
            spec.max_vcpus.unwrap_or(spec.boot_vcpus)
        ),
    ];
    if let Some(ref c) = spec.cmdline {
        args.push("--cmdline".into());
        args.push(c.clone());
    }
    if let Some(ref i) = spec.initramfs {
        args.push("--initramfs".into());
        args.push(i.clone());
    }
    if let Some(max_mem) = spec.max_memory_mb {
        args.push("--memory".into());
        args.push(format!(
            "size={}M,hotplug_method=virtio-mem,hotplug_size={}G",
            spec.memory_mb,
            max_mem / 1024
        ));
    } else {
        args.push("--memory".into());
        args.push(format!("size={}M", spec.memory_mb));
    }
    for disk in &spec.disks {
        args.push("--disk".into());
        args.push(format!("path={}", disk));
    }
    args.push("--serial".into());
    args.push("null".into());
    args.push("--console".into());
    args.push("off".into());
    args
}

/// Retry vm_info() up to 10 times with 500ms back-off.
/// CH API may return transient errors during startup.
async fn retry_get_info(client: &ChClient) -> Result<api::VmDetails, AdapterError> {
    for attempt in 0..10 {
        match client.vm_info().await {
            Ok(details) => return Ok(details),
            Err(e) => {
                if attempt == 9 {
                    return Err(AdapterError::internal(format!(
                        "vm.info failed after 10 attempts: {}",
                        e
                    )));
                }
                sleep(Duration::from_millis(500)).await;
            }
        }
    }
    unreachable!()
}

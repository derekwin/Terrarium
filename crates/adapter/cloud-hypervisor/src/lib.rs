//! Cloud Hypervisor adapter — self-contained VmAdapter implementation.
//!
//! Contains the CH HTTP API client (Unix socket) and VmAdapter trait impl.
//! No external CH SDK required — users install the official CH release binary.

pub mod api;
pub mod client;
mod config;
mod error;
mod fs;

use crate::fs::{compose_fs, teardown_fs, FsStack};
use adapter_traits::{
    AdapterError, FsSpec, NetworkQos, Snapshot, VmAdapter, VmCapabilities, VmHandle, VmInfo,
    VmName, VmSpec,
};
use async_trait::async_trait;
use config::ChConfig;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, Instant};

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

/// Next free vsock CID (0/1/2 reserved: hypervisor/host/local).
static NEXT_VSOCK_CID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(3);

/// Sanitize a VM name into a kernel-safe interface name (<= 15 chars).
fn tap_name(name: &str) -> String {
    let mut t: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    t.truncate(9); // "terra-" + 9 = 15 max
    t
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

        // CH stderr goes to a per-VM log file — invisible deaths (like
        // landlock denials) must be diagnosable.
        let log_dir = format!("{}/logs", adapter.config.fs_root);
        let _ = std::fs::create_dir_all(&log_dir);
        let log_path = format!("{}/{}.log", log_dir, name);
        let log_file = std::fs::File::create(&log_path)
            .map_err(|e| format!("create CH log {}: {}", log_path, e))?;

        let mut child = Command::new(&adapter.config.ch_binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ch_args(
    spec: &VmSpec,
    socket: &str,
    fs_socket: Option<&str>,
    vsock: &str,
    tap: Option<&str>,
) -> Vec<String> {
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
    // vhost-user devices (virtiofs) require shared guest memory. Always
    // on: any VM may receive a hot-plugged fs later (warm pool), and
    // shared memory is also the DAX/zero-copy path — no downside.
    let shared = ",shared=on";
    if let Some(max_mem) = spec.max_memory_mb {
        args.push("--memory".into());
        args.push(format!(
            "size={}M,hotplug_method=virtio-mem,hotplug_size={}G{}",
            spec.memory_mb,
            max_mem / 1024,
            shared
        ));
    } else {
        args.push("--memory".into());
        args.push(format!("size={}M{}", spec.memory_mb, shared));
    }
    if let Some(fs_sock) = fs_socket {
        args.push("--fs".into());
        args.push(format!("tag=rootfs,socket={},num_queues=1", fs_sock));
    }
    if let Some(tap) = tap {
        args.push("--net".into());
        args.push(format!("tap={}", tap));
    }
    // Landlock whitelists only cmdline paths; CH opens /dev/net/tun to
    // attach tap devices, so it must be granted explicitly when
    // networking is enabled (otherwise CH dies right after boot).
    if tap.is_some() {
        // CH opens /dev/net/tun to create/attach taps and reads the tap
        // flags from sysfs (/sys/class/net is a symlink farm into
        // /sys/devices/virtual/net — grant both, read-only).
        args.push("--landlock-rules".into());
        args.push("path=/dev/net/tun,access=rw".into());
        args.push("--landlock-rules".into());
        args.push("path=/sys/class/net,access=r".into());
        args.push("--landlock-rules".into());
        args.push("path=/sys/devices/virtual/net,access=r".into());
    }
    // vsock for host→guest control (guest-proxy); unique CID per VM.
    let cid = NEXT_VSOCK_CID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    args.push("--vsock".into());
    args.push(format!("cid={},socket={}", cid, vsock));
    args.push("--serial".into());
    args.push("null".into());
    args.push("--console".into());
    args.push("off".into());
    // Landlock confines the CH process to the paths explicitly given on
    // the command line (kernel/initramfs/api socket/fs socket) — anything
    // the VMM might be tricked into opening outside that set is denied.
    args.push("--landlock".into());
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

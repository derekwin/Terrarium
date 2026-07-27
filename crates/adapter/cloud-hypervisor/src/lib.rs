//! Cloud Hypervisor adapter — self-contained VmAdapter implementation.
//!
//! Contains the CH HTTP API client (Unix socket) and VmAdapter trait impl.
//! No external CH SDK required — users install the official CH release binary.

pub mod api;
pub mod client;
mod error;

use adapter_traits::{
    AdapterError, FsSpec, NetworkQos, Snapshot, UpperPolicy, VmAdapter, VmCapabilities, VmHandle,
    VmInfo, VmName, VmSpec,
};
use async_trait::async_trait;
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
    virtiofsd_binary: String,
    layer_dir: String,
    fs_root: String,
    /// EROFS layer images already mounted (shared across VMs; layers are
    /// immutable, mounts live for the daemon's lifetime).
    mounted_layers: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl ChAdapter {
    pub fn new(ch_binary: impl Into<String>) -> Self {
        let fs_base = std::env::var("TERRA_STATE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/tmp/terra-disks".into());
        Self {
            ch_binary: ch_binary.into(),
            // qemu's virtiofsd (apt) and rust-vmm's (cargo) share the CLI.
            virtiofsd_binary: std::env::var("TERRA_VIRTIOFSD")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "virtiofsd".into()),
            layer_dir: std::env::var("TERRA_LAYER_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/var/lib/terra/layers".into()),
            fs_root: format!("{}/fs", fs_base),
            mounted_layers: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Resolve a layer name to a usable lowerdir path.
    ///
    /// Resolution order: `<layer_dir>/<name>` directory first, then
    /// `<layer_dir>/<name>.erofs` image (mounted on first use). EROFS
    /// mounts are shared by all VMs and kept for the daemon's lifetime.
    fn resolve_layer(&self, name: &str) -> Result<String, AdapterError> {
        let dir = format!("{}/{}", self.layer_dir, name);
        if std::path::Path::new(&dir).is_dir() {
            return Ok(dir);
        }
        let image = format!("{}/{}.erofs", self.layer_dir, name);
        if !std::path::Path::new(&image).exists() {
            return Err(AdapterError::not_found(format!(
                "layer '{}' not found under {} (neither directory nor .erofs image)",
                name, self.layer_dir
            )));
        }
        let mnt = format!("{}/layers-mnt/{}", self.fs_root, name);
        // Already mounted? /proc/mounts is authoritative (survives
        // daemon restarts; EROFS mounts are read-only so no marker file
        // can be written into the mountpoint itself).
        if is_mounted(&mnt) {
            return Ok(mnt);
        }
        let mut set = self
            .mounted_layers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if set.contains(name) {
            return Ok(mnt);
        }
        std::fs::create_dir_all(&mnt).map_err(|e| format!("mkdir {}: {}", mnt, e))?;
        mount_erofs(&image, &mnt)?;
        set.insert(name.to_string());
        Ok(mnt)
    }
}

/// Whether `mnt` is an active mountpoint according to /proc/mounts.
fn is_mounted(mnt: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|c| c.lines().any(|l| l.split(' ').nth(1) == Some(mnt)))
        .unwrap_or(false)
}

/// Mount an EROFS image read-only at `mnt`. Kernel loop mount when
/// privileged, erofsfuse fallback otherwise.
fn mount_erofs(image: &str, mnt: &str) -> Result<(), AdapterError> {
    // Try kernel mount first (root path — best performance).
    let kernel = Command::new("mount")
        .args(["-o", "loop,ro", "-t", "erofs", image, mnt])
        .output();
    if let Ok(out) = kernel {
        if out.status.success() {
            tracing::info!(%image, %mnt, "EROFS layer mounted (kernel)");
            return Ok(());
        }
    }
    // Unprivileged fallback: erofsfuse.
    let fuse_bin = std::env::var("TERRA_EROFSFUSE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "erofsfuse".into());
    let out = Command::new(&fuse_bin)
        .args([image, mnt])
        .output()
        .map_err(|e| format!("mount failed (need root) and erofsfuse not found: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "erofsfuse {}: {}",
            image,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    tracing::info!(%image, %mnt, "EROFS layer mounted (erofsfuse)");
    Ok(())
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
    fs_config: ChAdapter,
}

/// Next free vsock CID (0/1/2 reserved: hypervisor/host/local).
static NEXT_VSOCK_CID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(3);

/// A composed layered rootfs: overlayfs mount + virtiofsd, running inside
/// a private user/mount namespace so the whole stack (including the
/// mount) dies with the supervisor process — no privileged cleanup needed.
struct FsStack {
    supervisor: std::process::Child,
    socket: String,
    /// Working dir root for this VM (upper/work/merged).
    dir: String,
    /// Persistent upperdirs live outside `dir` and survive Drop.
    persistent: bool,
}

impl ChVmHandle {
    async fn spawn(spec: &VmSpec, adapter: &ChAdapter) -> Result<Self, AdapterError> {
        let name = spec.name.clone();
        let socket = format!("/tmp/terra-{}.sock", name);
        let _ = std::fs::remove_file(&socket);

        // Compose the layered rootfs first — CH needs the virtiofsd
        // socket at boot.
        let fs = match spec.fs {
            Some(ref fs_spec) => Some(compose_fs(fs_spec, name.as_ref(), adapter).await?),
            None => None,
        };
        let fs_socket = fs.as_ref().map(|f| f.socket.as_str());

        let vsock = format!("/tmp/terra-{}-vsock.sock", name);
        let _ = std::fs::remove_file(&vsock);
        let args = ch_args(spec, &socket, fs_socket, &vsock);

        tracing::info!(name = %name, socket = %socket, layered = fs.is_some(), "Spawning CH VM");

        let mut child = Command::new(&adapter.ch_binary)
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
            fs: std::sync::Mutex::new(fs),
            fs_device_id: std::sync::Mutex::new(None),
            fs_config: ChAdapter {
                ch_binary: adapter.ch_binary.clone(),
                virtiofsd_binary: adapter.virtiofsd_binary.clone(),
                layer_dir: adapter.layer_dir.clone(),
                fs_root: adapter.fs_root.clone(),
                mounted_layers: std::sync::Mutex::new(std::collections::HashSet::new()),
            },
        })
    }
}

/// Compose a layered rootfs: resolve layer names under the layer dir,
/// overlayfs-mount them with a per-VM upperdir, and serve the result via
/// virtiofsd — all inside one `unshare -Urm` supervisor so no root is
/// required and teardown is just killing the process.
async fn compose_fs(
    fs_spec: &FsSpec,
    name: &str,
    adapter: &ChAdapter,
) -> Result<FsStack, AdapterError> {
    if fs_spec.layers.is_empty() {
        return Err(AdapterError::invalid_argument(
            "fs.layers must not be empty".to_string(),
        ));
    }
    let mut lowers: Vec<String> = Vec::new();
    for layer in &fs_spec.layers {
        if !layer
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(AdapterError::invalid_argument(format!(
                "invalid layer name {:?}",
                layer
            )));
        }
        // Resolves plain dirs directly and mounts .erofs images on demand.
        lowers.push(adapter.resolve_layer(layer)?);
    }
    // OverlayFS lowerdir is right-to-left priority: our layers list is
    // highest-priority-first, base last — join as-is.
    let lowerdir = lowers.join(":");

    let dir = format!("{}/{}", adapter.fs_root, name);
    let (upper, persistent) = match &fs_spec.upper {
        UpperPolicy::Ephemeral => (format!("{}/upper", dir), false),
        UpperPolicy::Persistent(pname) => {
            if !pname
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            {
                return Err(AdapterError::invalid_argument(format!(
                    "invalid upper name {:?}",
                    pname
                )));
            }
            (format!("{}/uppers/{}", adapter.fs_root, pname), true)
        }
    };
    let work = format!("{}/work", dir);
    let merged = format!("{}/merged", dir);
    for d in [&upper, &work, &merged] {
        std::fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {}", d, e))?;
    }

    let socket = format!("/tmp/terra-{}-fs.sock", name);
    let _ = std::fs::remove_file(&socket);
    // qemu virtiofsd creates a locked pid file next to the socket —
    // clear leftovers from previous (possibly crashed) stacks.
    let _ = std::fs::remove_file(format!("{}.pid", socket));

    let script = format!(
        "set -e; mount -t overlay overlay -o lowerdir={},upperdir={},workdir={} {}; \
         exec {} --socket-path={} --shared-dir={} --sandbox=none --cache=always",
        lowerdir, upper, work, merged, adapter.virtiofsd_binary, socket, merged
    );
    let mut child = Command::new("unshare")
        .args(["-Urm", "bash", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn unshare supervisor: {}", e))?;

    // Wait for the virtiofsd socket; surface supervisor stderr on failure.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::path::Path::new(&socket).exists() {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let mut err = String::new();
            use std::io::Read;
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            return Err(format!(
                "fs supervisor exited ({}) before virtiofsd was ready: {}",
                status,
                err.trim()
            )
            .into());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("virtiofsd socket timeout".into());
        }
        sleep(Duration::from_millis(100)).await;
    }

    tracing::info!(name = %name, layers = ?fs_spec.layers, %persistent, "Layered rootfs composed");
    Ok(FsStack {
        supervisor: child,
        socket,
        dir,
        persistent,
    })
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

    async fn attach_fs(&self, fs_spec: &FsSpec) -> Result<(), AdapterError> {
        if self.fs.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            return Err(AdapterError::internal(
                "an fs stack is already attached to this VM".to_string(),
            ));
        }
        // 1) compose layers + start virtiofsd on the host
        let stack = compose_fs(fs_spec, self.name.as_ref(), &self.fs_config).await?;
        // 2) hot-plug the virtiofs device
        let device_id = self
            .client
            .vm_add_fs("rootfs", &stack.socket)
            .await
            .map_err(|e| AdapterError::internal(format!("vm.add-fs: {}", e)))?;
        // 3) mount inside the guest via guest-proxy vsock. The guest
        // agent may still be booting — retry briefly.
        let mut resp = serde_json::Value::Null;
        #[allow(unused_assignments)]
        let mut last_err = String::new();
        for attempt in 0..20 {
            match self
                .guest_cmd(&serde_json::json!({
                    "command": "mount", "tag": "rootfs", "target": "/newroot",
                }))
                .await
            {
                Ok(r) => {
                    resp = r;
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    if attempt == 19 {
                        return Err(AdapterError::internal(format!(
                            "guest mount unreachable after retries: {}",
                            last_err
                        )));
                    }
                    sleep(Duration::from_millis(500)).await;
                }
            }
        }
        if resp["status"].as_str() != Some("ok") {
            return Err(AdapterError::internal(format!(
                "guest mount failed: {}",
                resp["message"].as_str().unwrap_or("unknown")
            )));
        }
        *self.fs_device_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(device_id);
        *self.fs.lock().unwrap_or_else(|e| e.into_inner()) = Some(stack);
        tracing::info!(name = %self.name, "fs attached (hot-plug)");
        Ok(())
    }

    async fn detach_fs(&self) -> Result<(), AdapterError> {
        // 1) best-effort guest umount
        let _ = self
            .guest_cmd(&serde_json::json!({"command": "umount", "target": "/newroot"}))
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

/// Tear down a composed fs stack: kill the supervisor (the overlayfs
/// mount and virtiofsd die with its namespace) and clean work dirs.
fn teardown_fs(fs: &mut FsStack) {
    let _ = fs.supervisor.kill();
    let _ = fs.supervisor.wait();
    let _ = std::fs::remove_file(&fs.socket);
    let _ = std::fs::remove_file(format!("{}.pid", fs.socket));
    if !fs.persistent {
        // overlayfs creates its internal work/work dir with mode 0000 —
        // restore owner permissions before removing.
        let _ = Command::new("chmod")
            .args(["-R", "u+rwX", &fs.dir])
            .output();
        if let Err(e) = std::fs::remove_dir_all(&fs.dir) {
            tracing::warn!(dir = %fs.dir, error = %e, "fs work dir cleanup failed");
        }
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
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ch_args(spec: &VmSpec, socket: &str, fs_socket: Option<&str>, vsock: &str) -> Vec<String> {
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

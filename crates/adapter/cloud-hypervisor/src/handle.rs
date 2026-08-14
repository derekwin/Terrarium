//! ChVmHandle — spawned VM handle, VmHandle trait impl, and lifecycle glue.
//!
//! Wires together fs (filesystem composition), process (CH spawning), and
//! config to implement the full VmHandle contract.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adapter_traits::{AdapterError, FsSpec, Snapshot, VmHandle, VmInfo, VmName, VmSpec};
use async_trait::async_trait;
use tokio::time::sleep;

use crate::client::ChClient;
use crate::config::ChConfig;
use crate::fs::{compose_fs, copy_tree, teardown_fs, FsStack};
use crate::process::{
    ch_args, ch_restore_args, retry_get_info, spawn_ch, tap_name, wait_for_socket,
};

// ---------------------------------------------------------------------------
// Public API — called from the adapter
// ---------------------------------------------------------------------------

/// Spawn a CH VM and return a trait-object [`VmHandle`].
pub(crate) async fn spawn_vm(
    spec: &VmSpec,
    config: Arc<ChConfig>,
) -> Result<Box<dyn VmHandle>, AdapterError> {
    ChVmHandle::launch(spec, config, None)
        .await
        .map(|h| Box::new(h) as Box<dyn VmHandle>)
}

/// Restore a CH VM from a snapshot (P1 fast reset) and return a
/// trait-object [`VmHandle`].
pub(crate) async fn restore_vm(
    snapshot: &Snapshot,
    spec: &VmSpec,
    config: Arc<ChConfig>,
) -> Result<Box<dyn VmHandle>, AdapterError> {
    ChVmHandle::launch(spec, config, Some(snapshot))
        .await
        .map(|h| Box::new(h) as Box<dyn VmHandle>)
}

// ---------------------------------------------------------------------------
// VmHandle
// ---------------------------------------------------------------------------

struct ChVmHandle {
    name: VmName,
    /// Host-side vsock socket the guest agent listens through; restored
    /// VMs use the path written into the snapshot config (per-VM).
    vsock_path: String,
    /// Per-restore snapshot directory (config rewritten to this VM's
    /// sockets); removed on Drop.
    restore_dir: Option<PathBuf>,
    /// CH subprocess; behind a Mutex so `is_alive(&self)` can `try_wait`
    /// without exclusive `&mut` access (reap must work while background
    /// exec tasks hold a second handle Arc).
    child: Mutex<Child>,
    client: ChClient,
    fs: Mutex<Option<FsStack>>,
    /// Device id of a hot-plugged fs device (needed for remove-device).
    fs_device_id: Mutex<Option<String>>,
    /// Adapter configuration shared behind an Arc.
    config: Arc<ChConfig>,
}

impl ChVmHandle {
    /// Build a per-restore snapshot directory: copies `config.json` +
    /// `state.json` and hardlinks `memory-ranges`, then rewrites the
    /// copy's device sockets to this VM's name-based paths. Each restore
    /// gets an isolated config, so parallel restores of one snapshot
    /// cannot race on the shared `config.json`.
    fn prepare_restore_dir(snapshot: &Snapshot, name: &str) -> Result<String, AdapterError> {
        let restore_dir = format!("{}/restore-{}", snapshot.path, name);
        let _ = std::fs::remove_dir_all(&restore_dir);
        std::fs::create_dir_all(&restore_dir)
            .map_err(|e| AdapterError::internal(format!("mkdir restore dir: {}", e)))?;
        for f in ["config.json", "state.json"] {
            std::fs::copy(
                format!("{}/{}", snapshot.path, f),
                format!("{}/{}", restore_dir, f),
            )
            .map_err(|e| AdapterError::internal(format!("copy snapshot {}: {}", f, e)))?;
        }
        // memory-ranges is the bulk (256MB+); hardlink it (same snapshot
        // dir tree) so the per-restore dir is cheap.
        let mem_src = format!("{}/memory-ranges", snapshot.path);
        let mem_dst = format!("{}/memory-ranges", restore_dir);
        if let Err(_e) = std::fs::hard_link(&mem_src, &mem_dst) {
            std::fs::copy(&mem_src, &mem_dst).map_err(|e| {
                AdapterError::internal(format!("copy snapshot memory-ranges: {}", e))
            })?;
        }

        let cfg_path = format!("{}/config.json", restore_dir);
        let raw = std::fs::read_to_string(&cfg_path)
            .map_err(|e| AdapterError::internal(format!("read snapshot config: {}", e)))?;
        let mut cfg: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| AdapterError::internal(format!("parse snapshot config: {}", e)))?;
        let fs_socket = format!("/tmp/terra-{}-fs.sock", name);
        let vsock_socket = format!("/tmp/terra-{}-vsock.sock", name);
        let tap = format!("terra-{}", tap_name(name));
        if let Some(fs) = cfg.get_mut("fs").and_then(|f| f.as_array_mut()) {
            for entry in fs.iter_mut() {
                if let Some(sock) = entry.get_mut("socket") {
                    *sock = serde_json::json!(fs_socket);
                }
            }
        }
        if let Some(sock) = cfg.get_mut("vsock").and_then(|v| v.get_mut("socket")) {
            *sock = serde_json::json!(vsock_socket);
        }
        // The restored config.json carries the ORIGINAL VM's net device
        // (including its tap name). CH restore re-attaches devices from
        // config.json — without this rewrite the restored VM would open
        // the snapshotted VM's tap, which is still owned by its live CH
        // ("Resource busy", observed on every net restore). Point it at
        // the fresh tap launch() created for this restored VM.
        if let Some(net) = cfg.get_mut("net").and_then(|n| n.as_array_mut()) {
            for entry in net.iter_mut() {
                if let Some(t) = entry.get_mut("tap") {
                    *t = serde_json::json!(tap);
                }
            }
        }
        let out = serde_json::to_string_pretty(&cfg)
            .map_err(|e| AdapterError::internal(format!("serialize snapshot config: {}", e)))?;
        std::fs::write(&cfg_path, out)
            .map_err(|e| AdapterError::internal(format!("write snapshot config: {}", e)))?;
        Ok(restore_dir)
    }

    /// Boot (`restore` = None) or restore-from-snapshot (`restore` = Some)
    /// a CH VM. The host-side stack (layers, vsock, api socket, CH
    /// subprocess) is built identically either way; only the guest payload
    /// differs (kernel boot vs `--restore`).
    pub(crate) async fn launch(
        spec: &VmSpec,
        config: Arc<ChConfig>,
        restore: Option<&Snapshot>,
    ) -> Result<Self, AdapterError> {
        let name = spec.name.clone();
        let socket = format!("/tmp/terra-{}.sock", name);
        let _ = std::fs::remove_file(&socket);
        let vsock = format!("/tmp/terra-{}-vsock.sock", name);
        let _ = std::fs::remove_file(&vsock);

        // Restore: build a per-restore snapshot dir (isolated config, so
        // parallel restores of one snapshot don't race) and compose the
        // layered rootfs on the name-based socket the rewritten config
        // points at.
        let restore_dir = match restore {
            Some(snapshot) => Some(Self::prepare_restore_dir(snapshot, name.as_ref())?),
            None => None,
        };
        let fs_socket = format!("/tmp/terra-{}-fs.sock", name);
        let _ = std::fs::remove_file(&fs_socket);
        let fs = match spec.fs {
            Some(ref fs_spec) => Some(compose_fs(fs_spec, name.as_ref(), &config).await?),
            None => None,
        };
        // Restore: seed the fresh overlay's upper from the snapshot's
        // captured upper — the virtiofsd device-state reload needs the
        // files the guest had written before the snapshot (verified:
        // without them the state load fails with "file not found").
        if let Some(snapshot) = restore {
            let snap_upper = format!("{}/upper", snapshot.path);
            if Path::new(&snap_upper).is_dir() {
                if let Some(fs) = fs.as_ref() {
                    copy_tree(Path::new(&snap_upper), Path::new(&fs.upper)).map_err(|e| {
                        AdapterError::internal(format!("seed restore upper: {}", e))
                    })?;
                }
            }
        }

        // Networking: NAT bridge + per-VM tap (privileged; clear error).
        // These run BLOCKING `ip`/`ebtables` subprocesses — under high
        // parallel creation they must not occupy the tokio workers (that
        // starves the daemon's accept loop and the keep-alive wrapper /
        // SDK fallback take the wrong action). spawn_blocking keeps the
        // async workers free while the subprocesses run.
        let net_flag = spec.net;
        let tap = if net_flag {
            let name_for_tap = name.as_ref().to_string();
            let setup = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
                terrarium_network::ensure_nat_bridge(
                    terrarium_network::DEFAULT_BRIDGE,
                    terrarium_network::DEFAULT_GATEWAY,
                    terrarium_network::DEFAULT_PREFIX,
                )?;
                let tap = format!("terra-{}", tap_name(&name_for_tap));
                terrarium_network::ensure_tap(&tap, terrarium_network::DEFAULT_BRIDGE)?;
                Ok(Some(tap))
            })
            .await
            .map_err(|e| AdapterError::internal(format!("tap setup task: {e}")))?
            .map_err(AdapterError::internal)?;
            setup
        } else {
            None
        };

        let args = match restore {
            Some(_) => ch_restore_args(
                &socket,
                &format!("file://{}", restore_dir.as_deref().unwrap()),
            ),
            None => ch_args(
                spec,
                &socket,
                fs.as_ref().map(|f| f.socket.as_str()),
                &vsock,
                tap.as_deref(),
                &config.snapshot_dir,
            ),
        };

        tracing::info!(
            name = %name,
            socket = %socket,
            layered = fs.is_some(),
            restoring = restore.is_some(),
            "Spawning CH VM"
        );

        let log_dir = format!("{}/logs", config.fs_root);
        let mut child = spawn_ch(&args, &config.ch_binary, &log_dir, name.as_ref())?;

        if let Err(e) = wait_for_socket(&socket, Duration::from_secs(15)).await {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }

        let client = ChClient::new(&socket).with_timeout(Duration::from_secs(5));
        tracing::info!(name = %name, "CH VM ready");

        Ok(Self {
            name,
            vsock_path: vsock,
            restore_dir: restore_dir.map(PathBuf::from),
            child: Mutex::new(child),
            client,
            fs: Mutex::new(fs),
            fs_device_id: Mutex::new(None),
            config,
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

    async fn exec(
        &self,
        opts: &adapter_traits::ExecOpts,
    ) -> Result<adapter_traits::ExecResult, AdapterError> {
        let mut req = serde_json::json!({
            "command": "exec", "args": &opts.args, "timeout_secs": opts.timeout_secs,
        });
        if opts.sandbox {
            req["sandbox"] = serde_json::Value::Bool(true);
        }
        if let Some(work_dir) = &opts.work_dir {
            req["work_dir"] = serde_json::Value::String(work_dir.to_string());
        }
        if let Some(exec_id) = &opts.exec_id {
            req["exec_id"] = serde_json::Value::String(exec_id.to_string());
        }
        if let Some(backend) = &opts.backend {
            req["backend"] = serde_json::Value::String(backend.to_string());
        }
        if let Some(policy) = &opts.policy {
            req["policy"] = serde_json::to_value(policy)
                .map_err(|e| AdapterError::internal(format!("serialize policy: {}", e)))?;
        }
        let resp = self.guest_cmd(&req).await?;
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

    async fn kill_exec(&self, exec_id: &str) -> Result<(), AdapterError> {
        // Fresh vsock connection per call (guest_cmd opens one).
        let resp = self
            .guest_cmd(&serde_json::json!({"command": "kill", "exec_id": exec_id}))
            .await?;
        if resp["status"].as_str() != Some("ok") {
            return Err(AdapterError::internal(format!(
                "guest kill failed: {}",
                resp["message"].as_str().unwrap_or("unknown")
            )));
        }
        Ok(())
    }

    async fn ping(&self) -> Result<(), AdapterError> {
        let resp = self
            .guest_cmd(&serde_json::json!({"command": "ping"}))
            .await?;
        if resp["status"].as_str() != Some("ok") {
            return Err(AdapterError::internal(format!(
                "guest ping failed: {}",
                resp["message"].as_str().unwrap_or("unknown")
            )));
        }
        Ok(())
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

    async fn snapshot(&self, path: &str) -> Result<Snapshot, AdapterError> {
        // CH writes the snapshot INTO a directory (memory + state files);
        // ensure it exists before pausing/capturing.
        std::fs::create_dir_all(path)
            .map_err(|e| AdapterError::internal(format!("mkdir snapshot dir: {}", e)))?;
        // CH only snapshots a paused VM. After capture the VM is LEFT
        // PAUSED: resume-after-snapshot leaves the guest unresponsive in
        // the CH builds we support, and the P1 reset flow (snapshot the
        // ready state → destroy → restore) never needs the source VM to
        // run again. On failure we resume so a failed snapshot does not
        // strand a paused VM.
        self.client
            .vm_pause()
            .await
            .map_err(|e| AdapterError::internal(format!("vm.pause: {}", e)))?;
        let result = self.client.vm_snapshot(path).await;
        if let Err(e) = result {
            let _ = self.client.vm_resume().await;
            return Err(AdapterError::internal(format!("vm.snapshot: {}", e)));
        }
        // Capture the fs upper alongside the CH snapshot — a restore
        // seeds its fresh overlay from this (the guest is paused here, so
        // the upper is quiescent).
        if let Ok(fs) = self.fs.lock() {
            if let Some(fs) = fs.as_ref() {
                if let Err(e) = copy_tree(Path::new(&fs.upper), &Path::new(path).join("upper")) {
                    return Err(AdapterError::internal(format!("capture fs upper: {}", e)));
                }
            }
        }
        Ok(Snapshot {
            path: path.to_string(),
        })
    }

    async fn reset_fs(&self) -> Result<(), AdapterError> {
        // Guest side: kill episode process groups + clear the
        // episode-writable runtime dirs back to the layer baseline.
        let resp = self
            .guest_cmd(&serde_json::json!({"command": "reset"}))
            .await?;
        if resp.get("status").and_then(|s| s.as_str()) != Some("ok") {
            return Err(AdapterError::internal(format!(
                "guest reset failed: {}",
                resp
            )));
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        self.client
            .vm_shutdown()
            .await
            .map_err(|e| AdapterError::internal(format!("vm.shutdown: {}", e)))
    }

    fn pid(&self) -> u32 {
        self.child.lock().unwrap_or_else(|e| e.into_inner()).id()
    }

    fn is_alive(&self) -> bool {
        matches!(
            self.child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .try_wait(),
            Ok(None)
        )
    }
}

impl ChVmHandle {
    /// Send one JSON command to guest-proxy over the CH vhost-vsock
    /// socket (text handshake "CONNECT <port>", then line-JSON).
    async fn guest_cmd(&self, cmd: &serde_json::Value) -> Result<serde_json::Value, AdapterError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::time::{timeout, Duration};
        let path = &self.vsock_path;

        // Host-side bound on the whole round-trip. The guest command runs
        // up to its own `timeout_secs`; give it headroom for scheduling.
        let cmd_timeout_secs = cmd
            .get("timeout_secs")
            .and_then(|t| t.as_u64())
            .unwrap_or(60);
        let total = Duration::from_secs(cmd_timeout_secs.saturating_add(15));

        // Handshake has a short fixed budget so a booting/absent guest
        // agent fails fast (callers retry on vsock/handshake errors)
        // instead of hanging the daemon forever.
        let connect = Duration::from_secs(10);
        let stream = timeout(connect, tokio::net::UnixStream::connect(&path))
            .await
            .map_err(|_| "vsock connect timeout: guest agent not ready".to_string())?
            .map_err(|e| format!("connect guest vsock: {}", e))?;
        let (reader, mut writer) = stream.into_split();
        timeout(connect, writer.write_all(b"CONNECT 1024\n"))
            .await
            .map_err(|_| "vsock CONNECT timeout: guest agent not ready".to_string())?
            .map_err(|e| format!("vsock CONNECT: {}", e))?;
        let mut lines = BufReader::new(reader).lines();
        let handshake = timeout(connect, lines.next_line())
            .await
            .map_err(|_| "vsock handshake timeout: guest agent not ready".to_string())?
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
        let resp = timeout(total, lines.next_line())
            .await
            .map_err(|_| {
                format!(
                    "guest command timed out after {}s (guest-proxy did not respond)",
                    cmd_timeout_secs.saturating_add(15)
                )
            })?
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
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
        drop(child);
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
        if let Some(dir) = &self.restore_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        // Remove the per-VM tap if networking was enabled (best-effort).
        let tap = format!("terra-{}", tap_name(self.name.as_ref()));
        let _ = terrarium_network::remove_tap(&tap);
    }
}

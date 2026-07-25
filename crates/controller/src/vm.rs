//! VmHandle — manages a single Cloud Hypervisor VM process and its API client.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use adapter_ch::api::VmDetails;
use adapter_ch::ChClient;

use crate::spec::VmSpec;

/// Error types for VM handle operations.
#[derive(Debug, thiserror::Error)]
pub enum VmError {
    #[error("VM '{name}' already exists")]
    AlreadyExists { name: String },

    #[error("VM '{name}' not found")]
    NotFound { name: String },

    #[error("Failed to spawn CH process: {0}")]
    SpawnFailed(#[source] std::io::Error),

    #[error("Setup failed: {0}")]
    SetupFailed(String),

    #[error("API socket did not appear within {timeout_ms}ms for VM '{name}'")]
    SocketTimeout { name: String, timeout_ms: u64 },

    #[error("CH client error for VM '{name}': {source}")]
    ClientError {
        name: String,
        #[source]
        source: adapter_ch::ClientError,
    },

    #[error("VM '{name}' process exited with status {status}\n--- stderr ---\n{stderr}\n---")]
    ProcessExited {
        name: String,
        status: i32,
        stderr: String,
    },
}

pub type Result<T> = std::result::Result<T, VmError>;

/// A handle to a running VM: owns the CH child process and its API client.
pub struct VmHandle {
    name: String,
    child: Child,
    client: ChClient,
    spec: VmSpec,
    /// Path to qcow2 overlay disk (cleaned up on drop).
    overlay_disk: Option<String>,
}

impl std::fmt::Debug for VmHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmHandle")
            .field("name", &self.name)
            .field("pid", &self.child.id())
            .field("socket", &self.spec.api_socket_path())
            .finish()
    }
}

/// Read stderr from a child process. Returns empty string on failure.
fn read_child_stderr(child: &mut Child) -> String {
    child
        .stderr
        .as_mut()
        .and_then(|s| {
            let mut buf = String::new();
            s.read_to_string(&mut buf).ok().map(|_| buf)
        })
        .unwrap_or_default()
}

/// Create a qcow2 overlay disk via the overlay crate.
fn create_qcow2_overlay(spec: &VmSpec) -> std::result::Result<(String, bool), VmError> {
    let base = spec.base_disk.as_ref().unwrap();
    let mut ospec = overlay::OverlaySpec::new(&spec.name, base).disk_size_gb(spec.disk_size_gb);
    for tool in &spec.tool_layers {
        ospec = ospec.tool_layer(tool);
    }
    let already_exists = overlay::OverlayManager::exists(&ospec);
    let path =
        overlay::OverlayManager::create_or_reuse(&ospec).map_err(VmError::SetupFailed)?;
    Ok((path, already_exists))
}

/// Take a CH VM snapshot.
pub fn snapshot_vm(client: &ChClient, snapshot_path: &str) -> std::result::Result<(), VmError> {
    client
        .vm_snapshot(snapshot_path)
        .map_err(|source| VmError::ClientError {
            name: "snapshot".into(),
            source,
        })
}

impl VmHandle {
    /// Spawn a new CH VM process and wait for its API socket to become ready.
    ///
    /// The socket path is cleaned up before spawning to avoid stale-socket
    /// collisions from a previous run.
    pub fn spawn(spec: VmSpec) -> Result<Self> {
        let name = spec.name.clone();
        let socket = spec.api_socket_path();
        let mut args = spec.to_ch_args();

        // Set up qcow2 overlay disk if base_disk is configured
        let mut overlay_disk: Option<String> = None;
        if spec.base_disk.is_some() {
            let (path, _existed) = create_qcow2_overlay(&spec)?;
            args.push("--disk".to_string());
            args.push(format!("path={}", path));
            overlay_disk = Some(path);
        }

        // Remove stale socket from a previous run
        let _ = std::fs::remove_file(&socket);

        tracing::info!(name = %name, socket = %socket, "Spawning VM");
        tracing::debug!(?args, "CH arguments");

        let mut child = Command::new(&spec.ch_binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(VmError::SpawnFailed)?;

        // Wait for the API socket to appear (with timeout)
        let socket_timeout = Duration::from_secs(10);
        let start = Instant::now();
        loop {
            // Check if process died while we were waiting
            match child.try_wait() {
                Ok(Some(status)) => {
                    let stderr = read_child_stderr(&mut child);
                    return Err(VmError::ProcessExited {
                        name,
                        status: status.code().unwrap_or(-1),
                        stderr,
                    });
                }
                Ok(None) => {} // Still running
                Err(e) => {
                    let _ = child.kill();
                    return Err(VmError::SpawnFailed(e));
                }
            }

            if std::path::Path::new(&socket).exists() {
                break;
            }

            if start.elapsed() > socket_timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(VmError::SocketTimeout {
                    name,
                    timeout_ms: socket_timeout.as_millis() as u64,
                });
            }
            thread::sleep(Duration::from_millis(100));
        }

        // Socket file appeared, but CH's HTTP server may not be ready yet.
        // Poll with a short-lived connection attempt until it responds or
        // the process dies (which would indicate a startup failure like
        // missing KVM access).
        let client = ChClient::new(&socket).with_timeout(Duration::from_secs(2));
        let poll_timeout = Duration::from_secs(15);
        let poll_start = Instant::now();
        loop {
            match client.vm_info() {
                Ok(_) => {
                    tracing::info!(name = %name, "VM spawned and API ready");
                    break;
                }
                Err(_) => {
                    // Check if CH died (e.g. KVM permission denied)
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let stderr = read_child_stderr(&mut child);
                            return Err(VmError::ProcessExited {
                                name,
                                status: status.code().unwrap_or(-1),
                                stderr,
                            });
                        }
                        Ok(None) => {} // Still trying to start
                        Err(e) => {
                            let _ = child.kill();
                            return Err(VmError::SpawnFailed(e));
                        }
                    }

                    if poll_start.elapsed() > poll_timeout {
                        return Err(VmError::SocketTimeout {
                            name,
                            timeout_ms: poll_timeout.as_millis() as u64,
                        });
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
        tracing::info!(name = %name, "VM spawned and API socket ready");

        Ok(Self {
            name,
            child,
            client,
            spec,
            overlay_disk,
        })
    }

    /// Return the VM name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return a reference to the VM spec.
    #[allow(dead_code)]
    pub fn spec(&self) -> &VmSpec {
        &self.spec
    }

    /// Return the CH child process ID.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Return a reference to the ChClient for direct API calls.
    pub fn client(&self) -> &ChClient {
        &self.client
    }

    /// Query VM info from the CH API. Retries on transient errors.
    pub fn info(&self) -> std::result::Result<VmDetails, VmError> {
        for attempt in 0..10 {
            match self.client.vm_info() {
                Ok(details) => return Ok(details),
                Err(e) => {
                    if attempt == 9 {
                        return Err(VmError::ClientError {
                            name: self.name.clone(),
                            source: e,
                        });
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
        unreachable!()
    }

    /// Resize vCPUs. Pass None to leave unchanged.
    pub fn resize_vcpus(&self, vcpus: Option<u8>) -> std::result::Result<(), VmError> {
        tracing::info!(name = %self.name, ?vcpus, "Resizing vCPUs");
        self.client
            .vm_resize(vcpus, None)
            .map_err(|source| VmError::ClientError {
                name: self.name.clone(),
                source,
            })
    }

    /// Resize memory (in bytes). Pass None to leave unchanged.
    pub fn resize_memory(&self, ram_bytes: Option<u64>) -> std::result::Result<(), VmError> {
        tracing::info!(name = %self.name, ram_mb = ram_bytes.map(|b| b / 1024 / 1024), "Resizing memory");
        self.client
            .vm_resize(None, ram_bytes)
            .map_err(|source| VmError::ClientError {
                name: self.name.clone(),
                source,
            })
    }

    /// Gracefully shut down the VM via the CH API, then wait for the process.
    pub fn shutdown(mut self) -> std::result::Result<(), VmError> {
        tracing::info!(name = %self.name, "Shutting down VM");
        let result = self
            .client
            .vm_shutdown()
            .map_err(|source| VmError::ClientError {
                name: self.name.clone(),
                source,
            });

        // Wait up to 10s for CH to exit gracefully.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if !self.is_alive() {
                break;
            }
            if Instant::now() > deadline {
                tracing::warn!(name = %self.name, "Shutdown wait timed out, force-killing");
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        let socket = self.spec.api_socket_path();
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(format!("{}.lock", socket));

        result
    }

    /// Force-kill the VM process (no graceful shutdown).
    pub fn kill(mut self) -> std::result::Result<(), VmError> {
        tracing::warn!(name = %self.name, "Force-killing VM");
        self.child.kill().map_err(VmError::SpawnFailed)?;
        let _ = self.child.wait();

        let socket = self.spec.api_socket_path();
        let _ = std::fs::remove_file(&socket);

        Ok(())
    }

    /// Check if the VM process is still running.
    pub fn is_alive(&self) -> bool {
        // Check /proc/<pid> as a non-invasive way to verify process existence.
        std::path::Path::new(&format!("/proc/{}", self.child.id())).exists()
    }
}

impl Drop for VmHandle {
    fn drop(&mut self) {
        if !self.is_alive() {
            return;
        }
        tracing::warn!(
            name = %self.name,
            "VmHandle dropped while VM still running — attempting shutdown"
        );
        let _ = self.client.vm_shutdown();
        // Wait up to 5 seconds for graceful shutdown, then force-kill.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if !self.is_alive() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        tracing::warn!(name = %self.name, "Graceful shutdown timed out, force-killing");
        let _ = self.child.kill();
        let _ = self.child.wait();
        let socket = self.spec.api_socket_path();
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(format!("{}.lock", socket));
    }
}

impl VmHandle {
    /// Destroy the VM and delete its persistent overlay disk.
    pub fn destroy(self) -> std::result::Result<(), VmError> {
        let name = self.name.clone();
        let overlay = self.overlay_disk.clone();
        let result = self.shutdown(); // shutdown consumes self
                                      // Clean up after shutdown
        if let Some(disk) = overlay {
            let _ = std::fs::remove_file(&disk);
            let state_dir = std::env::var("TERRA_STATE_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/tmp/terra-disks/vms".to_string());
            let vm_dir = format!("{}/{}", state_dir, name);
            let _ = std::fs::remove_dir_all(&vm_dir);
            tracing::info!(%name, %disk, "Destroyed overlay disk");
        }
        result
    }
}

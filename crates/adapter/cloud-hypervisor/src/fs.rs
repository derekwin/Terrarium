//! Filesystem composition for layered rootfs (EROFS + OverlayFS + virtiofsd).
//!
//! Pure filesystem logic — no dependency on any VM or adapter types.

use adapter_traits::{AdapterError, FsSpec, UpperPolicy};
use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::{sleep, Instant};
use terrarium_fs::layer::resolve_layer;
use terrarium_fs::LayerConfig;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Configuration for filesystem composition.
pub struct FsConfig {
    pub layer_dir: String,
    pub virtiofsd_binary: String,
    pub fs_root: String,
    /// EROFS layer images already mounted (shared across VMs; layers are
    /// immutable, mounts live for the daemon's lifetime).
    pub mounted_layers: Arc<Mutex<HashSet<String>>>,
}

/// A composed layered rootfs: overlayfs mount + virtiofsd. As non-root
/// it runs inside a private user/mount namespace (killing the supervisor
/// tears down the mount too); as root it mounts directly (userns uid
/// mapping would make other users' layer files unwritable-nobody).
pub struct FsStack {
    pub supervisor: std::process::Child,
    pub socket: String,
    /// Working dir root for this VM (upper/work/merged).
    pub dir: String,
    /// Persistent upperdirs live outside `dir` and survive Drop.
    pub persistent: bool,
    /// True when composed inside a private namespace (non-root path).
    pub in_namespace: bool,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compose a layered rootfs: resolve layer names under the layer dir,
/// overlayfs-mount them with a per-VM upperdir, and serve the result via
/// virtiofsd — all inside one `unshare -Urm` supervisor so no root is
/// required and teardown is just killing the process.
pub async fn compose_fs(
    fs_spec: &FsSpec,
    name: &str,
    config: &FsConfig,
) -> Result<FsStack, AdapterError> {
    if fs_spec.layers.is_empty() {
        return Err(AdapterError::invalid_argument(
            "fs.layers must not be empty".to_string(),
        ));
    }
    let layer_cfg = LayerConfig {
        layer_dir: config.layer_dir.clone(),
        fs_root: config.fs_root.clone(),
        mounted_layers: config.mounted_layers.clone(),
    };
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
        lowers.push(resolve_layer(&layer_cfg, layer)?);
    }
    // OverlayFS lowerdir is right-to-left priority: our layers list is
    // highest-priority-first, base last — join as-is.
    let lowerdir = lowers.join(":");

    let dir = format!("{}/{}", config.fs_root, name);
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
            (format!("{}/uppers/{}", config.fs_root, pname), true)
        }
    };
    let work = format!("{}/work", dir);
    let merged = format!("{}/merged", dir);
    for d in [&upper, &work, &merged] {
        std::fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {}", d, e))?;
    }

    let socket = format!("/tmp/terra-{}-fs.sock", name);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(format!("{}.pid", socket));

    // SAFETY: geteuid is always safe to call.
    let in_namespace = unsafe { libc::geteuid() } != 0;
    let mut child = if in_namespace {
        let script = format!(
            "set -e; mount -t overlay overlay -o lowerdir={},upperdir={},workdir={} {}; \
             exec {} --socket-path={} --shared-dir={} --sandbox=none --cache=always",
            lowerdir, upper, work, merged, config.virtiofsd_binary, socket, merged
        );
        Command::new("unshare")
            .args(["-Urm", "bash", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn unshare supervisor: {}", e))?
    } else {
        let mount = Command::new("mount")
            .args([
                "-t",
                "overlay",
                "overlay",
                "-o",
                &format!("lowerdir={},upperdir={},workdir={}", lowerdir, upper, work),
                &merged,
            ])
            .output()
            .map_err(|e| format!("mount overlay: {}", e))?;
        if !mount.status.success() {
            return Err(format!(
                "mount overlay: {}",
                String::from_utf8_lossy(&mount.stderr).trim()
            )
            .into());
        }
        Command::new(&config.virtiofsd_binary)
            .args([
                &format!("--socket-path={}", socket),
                &format!("--shared-dir={}", merged),
                "--sandbox=none",
                "--cache=always",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn virtiofsd: {}", e))?
    };

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
        in_namespace,
    })
}

/// Tear down a composed fs stack: kill the supervisor (the overlayfs
/// mount and virtiofsd die with its namespace) and clean work dirs.
pub fn teardown_fs(fs: &mut FsStack) {
    let _ = fs.supervisor.kill();
    let _ = fs.supervisor.wait();
    let _ = std::fs::remove_file(&fs.socket);
    if !fs.in_namespace {
        let _ = Command::new("umount")
            .arg(format!("{}/merged", fs.dir))
            .output();
    }
    let _ = std::fs::remove_file(format!("{}.pid", fs.socket));
    if !fs.persistent {
        let _ = Command::new("chmod")
            .args(["-R", "u+rwX", &fs.dir])
            .output();
        if let Err(e) = std::fs::remove_dir_all(&fs.dir) {
            tracing::warn!(dir = %fs.dir, error = %e, "fs work dir cleanup failed");
        }
    }
}



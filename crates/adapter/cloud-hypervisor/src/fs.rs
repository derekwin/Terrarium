//! Filesystem composition for layered rootfs (EROFS + OverlayFS + virtiofsd).
//!
//! Pure filesystem logic — no dependency on any VM or adapter types.

use adapter_traits::{AdapterError, FsSpec, UpperPolicy};
use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::{sleep, Instant};

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
        lowers.push(resolve_layer(config, layer)?);
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

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Resolve a layer name to a usable lowerdir path.
///
/// Resolution order: `<layer_dir>/<name>` directory first, then
/// `<layer_dir>/<name>.erofs` image (mounted on first use). EROFS
/// mounts are shared by all VMs and kept for the daemon's lifetime.
fn resolve_layer(config: &FsConfig, name: &str) -> Result<String, AdapterError> {
    let dir = format!("{}/{}", config.layer_dir, name);
    if std::path::Path::new(&dir).is_dir() {
        return Ok(dir);
    }
    let image = format!("{}/{}.erofs", config.layer_dir, name);
    if !std::path::Path::new(&image).exists() {
        return Err(AdapterError::not_found(format!(
            "layer '{}' not found under {} (neither directory nor .erofs image)",
            name, config.layer_dir
        )));
    }
    let mnt = format!("{}/layers-mnt/{}", config.fs_root, name);
    let mtime = std::fs::metadata(&image)
        .and_then(|m| m.modified())
        .map(|t| format!("{:?}", t))
        .unwrap_or_default();
    let sidecar = format!("{}.mtime", mnt);
    let mounted_as = std::fs::read_to_string(&sidecar).unwrap_or_default();
    if is_mounted(&mnt) {
        if mounted_as == mtime {
            return Ok(mnt);
        }
        tracing::warn!(%mnt, "layer image rebuilt, remounting");
        let _ = Command::new("umount").arg(&mnt).output();
        let _ = Command::new("fusermount").args(["-u", &mnt]).output();
    }
    let mut set = config
        .mounted_layers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if set.contains(name) {
        return Ok(mnt);
    }
    std::fs::create_dir_all(&mnt).map_err(|e| format!("mkdir {}: {}", mnt, e))?;
    mount_erofs(&image, &mnt)?;
    let _ = std::fs::write(&sidecar, &mtime);
    set.insert(name.to_string());
    Ok(mnt)
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
    let kernel = Command::new("mount")
        .args(["-o", "loop,ro", "-t", "erofs", image, mnt])
        .output();
    if let Ok(out) = kernel {
        if out.status.success() {
            tracing::info!(%image, %mnt, "EROFS layer mounted (kernel)");
            return Ok(());
        }
    }
    let fuse_bin = std::env::var("TERRA_EROFSFUSE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            for c in [
                "erofsfuse".to_string(),
                format!(
                    "{}/.local/share/terra/bin/erofsfuse",
                    std::env::var("HOME").unwrap_or_default()
                ),
                "/usr/bin/erofsfuse".to_string(),
            ] {
                if std::path::Path::new(&c).exists() || c == "erofsfuse" {
                    return c;
                }
            }
            "erofsfuse".into()
        });
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

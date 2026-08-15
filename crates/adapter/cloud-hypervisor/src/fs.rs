//! Filesystem composition for layered rootfs (EROFS + OverlayFS + virtiofsd).
//!
//! Pure filesystem logic — no dependency on any VM or adapter types.

use adapter_traits::{AdapterError, FsSpec, UpperPolicy};
use std::collections::HashSet;
use std::ffi::CString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use terrarium_fs::layer::{resolve_layer, validate_layer_name};
use terrarium_fs::LayerConfig;
use tokio::time::{sleep, Instant};

use crate::config::VmUser;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Configuration for filesystem composition.
pub struct FsConfig {
    pub layer_dir: String,
    pub virtiofsd_binary: String,
    /// virtiofsd cache mode (`always` | `auto` | `none`). `auto` is the
    /// default so in-place episode reset (host-side upper replacement)
    /// is visible to the guest — `always` keeps stale dentries.
    pub virtiofsd_cache: String,
    pub fs_root: String,
    /// EROFS layer images already mounted (shared across VMs; layers are
    /// immutable, mounts live for the daemon's lifetime).
    pub mounted_layers: Arc<Mutex<HashSet<String>>>,
    /// Plain layer dirs already chowned to the vmm user (lazy, once per
    /// daemon lifetime — chown -R on a large layer is not per-VM work).
    pub chowned_layers: Arc<Mutex<HashSet<String>>>,
    /// Privilege-drop target for virtiofsd (None = legacy root mode).
    pub vmm: Option<VmUser>,
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
    /// The overlay upperdir — the writable layer the guest's runtime
    /// writes land in. Captured with a snapshot (P1 fast reset) so a
    /// restore can seed a fresh overlay with the same upper state.
    pub upper: String,
    /// Persistent upperdirs live outside `dir` and survive Drop.
    pub persistent: bool,
    /// True when composed inside a private namespace (non-root path).
    pub in_namespace: bool,
}

/// Recursively chown a path (symlinks via lchown). Used to hand managed
/// layer/upper/work trees to the vmm user so a non-root virtiofsd can
/// serve them. Read-only EROFS mounts are skipped by callers.
pub(crate) fn chown_r(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    let cpath = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: cpath is a valid NUL-terminated path; lchown/chown have no
    // other preconditions beyond a valid path.
    let rc = if meta.file_type().is_symlink() {
        unsafe { libc::lchown(cpath.as_ptr(), uid, gid) }
    } else {
        unsafe { libc::chown(cpath.as_ptr(), uid, gid) }
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_r(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
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
        validate_layer_name(layer).map_err(AdapterError::invalid_argument)?;
        lowers.push(resolve_layer(&layer_cfg, layer)?);
    }
    // OverlayFS lowerdir is right-to-left priority: our layers list is
    // highest-priority-first, base last — join as-is.
    let lowerdir = lowers.join(":");

    let dir = format!("{}/{}", config.fs_root, name);
    let (upper, persistent) = match &fs_spec.upper {
        UpperPolicy::Ephemeral => (format!("{}/upper", dir), false),
        UpperPolicy::Persistent(pname) => {
            validate_layer_name(pname).map_err(AdapterError::invalid_argument)?;
            (format!("{}/uppers/{}", config.fs_root, pname), true)
        }
    };
    let work = format!("{}/work", dir);
    let merged = format!("{}/merged", dir);
    for d in [&upper, &work, &merged] {
        std::fs::create_dir_all(d).map_err(|e| format!("mkdir {}: {}", d, e))?;
    }

    // Privilege-drop mode: hand the per-VM overlay dirs to the vmm user so
    // virtiofsd (running as that user) can serve the tree. Layer dirs are
    // chowned lazily once per daemon lifetime (large; not per-VM work);
    // read-only EROFS mounts are left alone (contents are packed
    // world-readable or force-owned by the vmm user at pack time).
    if let Some(vmm) = &config.vmm {
        for d in [&upper, &work, &merged] {
            chown_r(Path::new(d), vmm.uid, vmm.gid)
                .map_err(|e| format!("chown {} to {}: {}", d, vmm.name, e))?;
        }
        for lower in &lowers {
            if terrarium_fs::is_mounted(lower) {
                continue;
            }
            let already = config
                .chowned_layers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(lower);
            if already {
                continue;
            }
            chown_r(Path::new(lower), vmm.uid, vmm.gid)
                .map_err(|e| format!("chown layer {lower} to {}: {}", vmm.name, e))?;
            config
                .chowned_layers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(lower.clone());
        }
    }

    let socket = format!("/tmp/terra-{}-fs.sock", name);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(format!("{}.pid", socket));

    // SAFETY: geteuid is always safe to call.
    let in_namespace = unsafe { libc::geteuid() } != 0;
    let mut child = if in_namespace {
        let script = format!(
            "set -e; mount -t overlay overlay -o lowerdir={},upperdir={},workdir={} {}; \
             exec {} --socket-path={} --shared-dir={} --sandbox=none --cache={}",
            lowerdir,
            upper,
            work,
            merged,
            config.virtiofsd_binary,
            socket,
            merged,
            config.virtiofsd_cache
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
        let mut virtiofsd = Command::new(&config.virtiofsd_binary);
        virtiofsd.args([
            &format!("--socket-path={}", socket),
            &format!("--shared-dir={}", merged),
            "--sandbox=none",
            &format!("--cache={}", config.virtiofsd_cache),
        ]);
        // Non-root virtiofsd: translate the vmm user's ownership back to
        // guest root so the exported rootfs keeps normal root-owned
        // semantics (the vmm user is the sole owner of the export tree).
        if let Some(vmm) = &config.vmm {
            virtiofsd.args([
                &format!("--translate-uid=host:{}:0:1", vmm.uid),
                &format!("--translate-gid=host:{}:0:1", vmm.gid),
            ]);
            // SAFETY: pre_exec runs in the child after fork; setgroups/
            // setgid/setuid are async-signal-safe. Capture ids by value.
            let (uid, gid) = (vmm.uid, vmm.gid);
            unsafe {
                virtiofsd.pre_exec(move || {
                    // SAFETY: standard libc calls; no locks touched.
                    if libc::setgroups(0, std::ptr::null()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::setgid(gid) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::setuid(uid) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        virtiofsd
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
            return Err(fs_supervisor_failure(&status, &err).into());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("virtiofsd socket timeout".into());
        }
        // Sub-10ms poll: virtiofsd opens its socket a few ms after spawn,
        // so a 100ms quantum added ~100ms of pure polling latency to every
        // VM launch/restore (measured: compose_fs alone was ~106ms, most of
        // it this sleep). This is the fast-create hot path.
        sleep(Duration::from_millis(5)).await;
    }

    tracing::info!(name = %name, layers = ?fs_spec.layers, %persistent, "Layered rootfs composed");
    Ok(FsStack {
        supervisor: child,
        socket,
        dir,
        upper,
        persistent,
        in_namespace,
    })
}

/// Format an fs supervisor failure, appending an actionable hint when the
/// supervisor died on the classic unprivileged user-namespace block: the
/// `uid_map` write is denied with EPERM (Ubuntu's
/// `kernel.apparmor_restrict_unprivileged_userns`, or a container
/// seccomp/capability policy that forbids `unshare -Urm`).
fn fs_supervisor_failure(status: &std::process::ExitStatus, stderr: &str) -> String {
    let mut msg = format!(
        "fs supervisor exited ({}) before virtiofsd was ready: {}",
        status,
        stderr.trim()
    );
    if stderr.contains("uid_map") && stderr.contains("Operation not permitted") {
        msg.push_str(
            " — user/mount namespaces are blocked in this environment: check \
             kernel.apparmor_restrict_unprivileged_userns (needs to be 0) and that the \
             container grants CAP_SYS_ADMIN and allows unshare (seccomp/AppArmor)",
        );
    }
    msg
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

/// Recursively copy a directory tree, preserving symlinks. Regular files
/// and directories are copied; sockets/devices/fifos are skipped (they
/// cannot be re-created, and the restored guest recreates them).
pub(crate) fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&from)?;
            let _ = std::fs::remove_file(&to);
            std::os::unix::fs::symlink(&target, &to)?;
        } else if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to)?;
        } else {
            tracing::warn!(path = %from.display(), "skipping non-regular upper entry");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn userns_block_gets_an_actionable_hint() {
        let msg = fs_supervisor_failure(
            &std::process::ExitStatus::from_raw(1),
            "unshare: write failed /proc/self/uid_map: Operation not permitted",
        );
        assert!(
            msg.contains("apparmor_restrict_unprivileged_userns"),
            "{msg}"
        );
        assert!(msg.contains("CAP_SYS_ADMIN"), "{msg}");
    }

    #[test]
    fn unrelated_failure_stays_plain() {
        let msg = fs_supervisor_failure(
            &std::process::ExitStatus::from_raw(1),
            "virtiofsd: failed to open socket: No such file or directory",
        );
        assert!(!msg.contains("apparmor"), "{msg}");
        assert!(msg.contains("virtiofsd"), "{msg}");
    }
}

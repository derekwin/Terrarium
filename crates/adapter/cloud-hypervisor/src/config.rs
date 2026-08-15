//! ChConfig — adapter configuration extracted from ChAdapter.
//!
//! Wraps an [`FsConfig`] and adds the Cloud Hypervisor binary path.
//! Created once at daemon start and shared behind an `Arc` so every VM
//! handle sees the same mounted-layer cache.

use std::collections::HashSet;
use std::ffi::CString;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::fs::FsConfig;

/// Dedicated unprivileged user that runs Cloud Hypervisor and virtiofsd
/// when the daemon is root (L1 降权: the host-side data planes are not
/// root, so a guest escape into them does not immediately yield host root).
#[derive(Clone, Debug)]
pub struct VmUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// Resolve the VMM user from `TERRA_VMM_USER` (default `terra-vmm`).
///
/// Only active when the daemon runs as root: in rootless mode CH/virtiofsd
/// already run as the (unprivileged) daemon user inside `unshare -Urm`.
/// Returns `None` (with a warning) when the user is missing so existing
/// deployments keep working until `terra setup` creates it.
pub fn resolve_vmm_user() -> Option<VmUser> {
    let name = std::env::var("TERRA_VMM_USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "terra-vmm".into());
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    // SAFETY: getpwnam returns a pointer to static storage; we copy the
    // numeric ids before the CString goes out of scope.
    let cname = CString::new(name.as_str()).ok()?;
    let p = unsafe { libc::getpwnam(cname.as_ptr()) };
    if p.is_null() {
        tracing::warn!(
            user = %name,
            "vmm user missing — CH/virtiofsd keep running as root; run `terra setup` (or create the user) to enable privilege dropping"
        );
        return None;
    }
    // SAFETY: p is non-null, pw_uid/pw_gid are valid.
    let (uid, gid) = unsafe { ((*p).pw_uid, (*p).pw_gid) };
    tracing::info!(user = %name, uid, gid, "vmm privilege drop enabled");
    Some(VmUser { name, uid, gid })
}

/// Cloud Hypervisor adapter configuration.
///
/// Implements [`Deref`](std::ops::Deref) to [`FsConfig`] so it can be
/// passed wherever the filesystem composition layer expects [`FsConfig`].
pub struct ChConfig {
    pub ch_binary: String,
    /// Managed snapshot directory (P1 fast reset). CH writes snapshot
    /// memory/state files into subdirectories of this root, and the
    /// engine's `--landlock-rules` whitelist must cover it.
    pub snapshot_dir: String,
    /// Privilege-drop target for CH/virtiofsd (None = legacy root mode).
    pub vmm: Option<VmUser>,
    pub fs: FsConfig,
}

/// chown -R a daemon-managed root so the vmm user can traverse/write it.
/// Guarded: never chown `/` or `/tmp` (world-writable roots need no chown
/// and a recursive chown there would be destructive).
fn chown_managed_root(path: &str, vmm: &VmUser) {
    let p = Path::new(path);
    if p == Path::new("/") || p == Path::new("/tmp") {
        return;
    }
    let _ = crate::fs::chown_r(p, vmm.uid, vmm.gid);
}

impl ChConfig {
    /// Build configuration from environment variables.
    ///
    /// | env var           | default                  |
    /// |-------------------|--------------------------|
    /// | `TERRA_STATE_DIR` | `/tmp/terra-disks`       |
    /// | `TERRA_VIRTIOFSD` | `virtiofsd`              |
    /// | `TERRA_LAYER_DIR` | `/var/lib/terra/layers`  |
    /// | `TERRA_SNAPSHOT_DIR` | `/tmp`               |
    /// | `TERRA_VIRTIOFSD_CACHE` | `always`           |
    pub fn from_env(ch_binary: impl Into<String>) -> Self {
        let fs_base = std::env::var("TERRA_STATE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/tmp/terra-disks".into());
        let snapshot_dir = std::env::var("TERRA_SNAPSHOT_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/tmp".into());
        let vmm = resolve_vmm_user();
        if let Some(vmm_user) = &vmm {
            // Host-side roots the vmm user must reach: the per-VM fs
            // composition tree and the snapshot/restore directories. One
            // chown at daemon start; per-VM dirs are chowned on creation.
            // The state root itself is chowned (not -R) so the vmm user
            // can traverse into it even when it was root-created 0700.
            let _ = std::fs::create_dir_all(&fs_base);
            let _ = crate::fs::chown_r(Path::new(&fs_base), vmm_user.uid, vmm_user.gid);
            let fs_root = format!("{fs_base}/fs");
            let _ = std::fs::create_dir_all(&fs_root);
            chown_managed_root(&fs_root, vmm_user);
            chown_managed_root(&snapshot_dir, vmm_user);
        }
        Self {
            ch_binary: ch_binary.into(),
            snapshot_dir,
            vmm: vmm.clone(),
            fs: FsConfig {
                // qemu's virtiofsd (apt) and rust-vmm's (cargo) share the CLI.
                virtiofsd_binary: std::env::var("TERRA_VIRTIOFSD")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "virtiofsd".into()),
                // Tuneable; `always` is the default (max caching). The
                // in-place episode reset is guest-side (the guest removes
                // its own files through virtiofsd), so caching mode does
                // not affect reset correctness.
                virtiofsd_cache: std::env::var("TERRA_VIRTIOFSD_CACHE")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "always".into()),
                layer_dir: std::env::var("TERRA_LAYER_DIR")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "/var/lib/terra/layers".into()),
                fs_root: format!("{}/fs", fs_base),
                mounted_layers: Arc::new(Mutex::new(HashSet::new())),
                chowned_layers: Arc::new(Mutex::new(HashSet::new())),
                vmm: vmm.clone(),
            },
        }
    }
}

impl std::ops::Deref for ChConfig {
    type Target = FsConfig;

    fn deref(&self) -> &Self::Target {
        &self.fs
    }
}

//! CPIO layer manipulation: pack / extract / initramfs build.
//!
//! Layer directories can be archived as cpio.gz rootfs images or extracted
//! from them. Also builds initramfs images for both warm-pool agent (FS-M4)
//! and virtiofs bootstrap (FS-M1). Uses the host `cpio` and `gzip`/`zcat`
//! commands.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Pack a layer directory into a bootable cpio.gz rootfs image.
///
/// Runs `(cd <layer_dir> && find . | cpio -o -H newc --quiet | gzip)`
/// and writes the result to `<output_dir>/<name>.cpio.gz`.
pub fn pack_cpio_rootfs(layer_dir: &str, name: &str, output_dir: &str) -> Result<String, String> {
    let src = Path::new(layer_dir);
    if !src.is_dir() {
        return Err(format!("layer directory not found: {}", layer_dir));
    }
    let out = Path::new(output_dir).join(format!("{}.cpio.gz", name));

    pack_dir(src, &out)?;

    tracing::info!(%name, output = %out.display(), "cpio rootfs packed");
    Ok(out.to_string_lossy().to_string())
}

/// Extract a cpio.gz archive into a directory.
///
/// Runs `zcat <cpio_path> | (cd <output_dir> && cpio -idm --quiet)`.
pub fn extract_cpio_layer(cpio_path: &str, output_dir: &str) -> Result<(), String> {
    let dest = Path::new(output_dir);
    std::fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {}", output_dir, e))?;

    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "zcat {} | (cd {} && cpio -idm --quiet)",
            shell_escape(cpio_path),
            shell_escape(output_dir),
        ))
        .output()
        .map_err(|e| format!("extract cpio: {}", e))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(format!("cpio extract failed: {}", stderr.trim()));
    }

    tracing::info!(%cpio_path, %output_dir, "cpio layer extracted");
    Ok(())
}

/// Build a warm-pool agent initramfs (FS-M4): busybox + guest-proxy + init.
///
/// Creates a cpio.gz image containing busybox (with symlinks for sh, mount,
/// umount, mkdir, echo, cat, ls, ip, udhcpc), musl shared libraries, the
/// guest-proxy binary, and the agent init script.
pub fn build_initramfs_agent(
    src_rootfs_dir: &str,
    guest_proxy_binary: &str,
    init_template: &str,
    output: &str,
) -> Result<String, String> {
    let src = Path::new(src_rootfs_dir);
    if !src.is_dir() {
        return Err(format!("src_rootfs_dir not found: {}", src_rootfs_dir));
    }
    let gp = Path::new(guest_proxy_binary);
    if !gp.is_file() {
        return Err(format!(
            "guest_proxy_binary not found: {}",
            guest_proxy_binary
        ));
    }
    let init = Path::new(init_template);
    if !init.is_file() {
        return Err(format!("init_template not found: {}", init_template));
    }

    build_initramfs(
        "terrarium-agent-irfs",
        &["bin", "lib", "proc", "sys", "dev", "tmp"],
        &[
            "sh", "mount", "umount", "mkdir", "echo", "cat", "ls", "ip", "udhcpc",
        ],
        src,
        Some(gp),
        init,
        Path::new(output),
        "agent",
    )
}

/// Build a virtiofs bootstrap initramfs (FS-M1): busybox + init.
///
/// Creates a cpio.gz image containing busybox (with symlinks for sh, mount,
/// switch_root, mkdir, echo, cat), musl shared libraries, a newroot/
/// directory, and the virtiofs init script that mounts the host-shared
/// rootfs and switch_roots into it.
pub fn build_initramfs_virtiofs(
    src_rootfs_dir: &str,
    init_template: &str,
    output: &str,
) -> Result<String, String> {
    let src = Path::new(src_rootfs_dir);
    if !src.is_dir() {
        return Err(format!("src_rootfs_dir not found: {}", src_rootfs_dir));
    }
    let init = Path::new(init_template);
    if !init.is_file() {
        return Err(format!("init_template not found: {}", init_template));
    }

    build_initramfs(
        "terrarium-virtiofs-irfs",
        &["bin", "lib", "proc", "sys", "dev", "tmp", "newroot"],
        &["sh", "mount", "switch_root", "mkdir", "echo", "cat"],
        src,
        None,
        init,
        Path::new(output),
        "virtiofs",
    )
}

/// Shared initramfs build core: busybox + musl libs (+ optional guest-proxy)
/// + init script, packed to cpio.gz; `kind` only affects the tracing message.
#[allow(clippy::too_many_arguments)]
fn build_initramfs(
    temp_prefix: &str,
    subdirs: &[&str],
    symlinks: &[&str],
    src: &Path,
    guest_proxy: Option<&Path>,
    init: &Path,
    out: &Path,
    kind: &str,
) -> Result<String, String> {
    let work_dir = make_temp_dir(temp_prefix)?;

    // Create subdirectories
    for subdir in subdirs {
        std::fs::create_dir_all(work_dir.join(subdir))
            .map_err(|e| format!("mkdir {}/{}: {}", work_dir.display(), subdir, e))?;
    }

    // Copy busybox
    std::fs::copy(src.join("bin/busybox"), work_dir.join("bin/busybox"))
        .map_err(|e| format!("copy busybox: {e}"))?;

    // Create busybox symlinks
    for cmd in symlinks {
        let dest = work_dir.join("bin").join(cmd);
        if dest.exists() {
            std::fs::remove_file(&dest).map_err(|e| format!("remove {}: {}", dest.display(), e))?;
        }
        std::os::unix::fs::symlink("busybox", &dest)
            .map_err(|e| format!("symlink {cmd} -> busybox: {e}"))?;
    }

    // Copy musl libs (ld-musl-*.so.1, libc.musl-*.so.1)
    copy_musl_libs(src.join("lib"), work_dir.join("lib"))?;

    // Copy guest-proxy (agent initramfs only)
    if let Some(gp) = guest_proxy {
        std::fs::copy(gp, work_dir.join("bin/guest-proxy"))
            .map_err(|e| format!("copy guest-proxy: {e}"))?;
    }

    // Copy init template
    let init_dest = work_dir.join("init");
    std::fs::copy(init, &init_dest).map_err(|e| format!("copy init template: {e}"))?;

    // chmod +x init (and guest-proxy)
    let mut executables = vec![init_dest.clone()];
    if guest_proxy.is_some() {
        executables.push(work_dir.join("bin/guest-proxy"));
    }
    for file in &executables {
        let mut perms = std::fs::metadata(file)
            .map_err(|e| format!("stat {}: {e}", file.display()))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(file, perms)
            .map_err(|e| format!("chmod +x {}: {e}", file.display()))?;
    }

    // Pack into cpio.gz
    pack_work_dir(&work_dir, out)?;

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&work_dir);

    tracing::info!(output = %out.display(), "{kind} initramfs built");
    Ok(out.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn make_temp_dir(prefix: &str) -> Result<PathBuf, String> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{:x}",
        std::process::id(),
        ts.as_nanos()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create temp dir {}: {e}", dir.display()))?;
    Ok(dir)
}

fn copy_musl_libs(src: PathBuf, dest: PathBuf) -> Result<(), String> {
    for entry in std::fs::read_dir(&src).map_err(|e| format!("read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("read entry: {e}"))?;
        let fname = entry.file_name();
        let fname_str = fname.to_string_lossy();
        if fname_str.starts_with("ld-musl-") || fname_str.starts_with("libc.musl-") {
            std::fs::copy(entry.path(), dest.join(&*fname_str))
                .map_err(|e| format!("copy {fname_str}: {e}"))?;
        }
    }
    Ok(())
}

fn pack_work_dir(work_dir: &Path, output: &Path) -> Result<(), String> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    pack_dir(work_dir, output)
}

/// Shared pack core: `(cd <src> && find . | cpio -o -H newc --quiet | gzip)`
/// into a temp file next to `out`, renamed into place on success.
fn pack_dir(src: &Path, out: &Path) -> Result<(), String> {
    let tmp = out.with_extension("tmp");

    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd {} && find . | cpio -o -H newc --quiet | gzip > {}",
            shell_escape(&src.to_string_lossy()),
            shell_escape(&tmp.to_string_lossy()),
        ))
        .output()
        .map_err(|e| format!("pack cpio: {e}"))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cpio pack failed: {}", stderr.trim()));
    }

    std::fs::rename(&tmp, out).map_err(|e| format!("rename {tmp:?} -> {out:?}: {e}"))?;
    Ok(())
}

/// Minimal shell-escaping for filenames used in `sh -c` snippets.
/// Only escapes single-quotes; wraps in single quotes.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

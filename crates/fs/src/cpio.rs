//! CPIO layer manipulation: pack / extract.
//!
//! Layer directories can be archived as cpio.gz rootfs images or extracted
//! from them. Uses the host `cpio` and `gzip`/`zcat` commands.

use std::path::Path;
use std::process::Command;

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
    let tmp = out.with_extension("tmp");

    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd {} && find . | cpio -o -H newc --quiet | gzip > {}",
            shell_escape(layer_dir),
            shell_escape(&tmp.to_string_lossy()),
        ))
        .output()
        .map_err(|e| format!("pack cpio: {}", e))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cpio pack failed: {}", stderr.trim()));
    }

    std::fs::rename(&tmp, &out).map_err(|e| format!("rename {:?} -> {:?}: {}", tmp, out, e))?;

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

/// Minimal shell-escaping for filenames used in `sh -c` snippets.
/// Only escapes single-quotes; wraps in single quotes.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

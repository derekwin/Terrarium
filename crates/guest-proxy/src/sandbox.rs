//! Sandbox execution: spawn and capture process output.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

/// In-guest candidates for the sandlock confinement binary, probed in
/// order; the first that exists wins. Cold-boot VMs have composed layers
/// as the root fs ("/usr/bin/sandlock"); pool/hot-plug VMs boot from the
/// busybox initramfs with composed layers mounted at /workdir
/// ("/workdir/usr/bin/sandlock").
pub const SANDLOCK_PATHS: &[&str] = &["/usr/bin/sandlock", "/workdir/usr/bin/sandlock"];

/// Default v1 sandbox policy (per-session isolation in a shared tenant VM):
/// read-only system paths, read-write on the session work_dir and /tmp,
/// network unrestricted (no --net-allow flags). Deliberately does NOT
/// grant "/" — that would leak sibling sessions' workdirs under /workdir.
/// Paths that don't exist in the image (e.g. /lib64, /sbin on busybox)
/// are filtered out by the caller-supplied `exists` probe, because
/// sandlock errors on nonexistent grant paths.
const READ_GRANTS: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/sbin",
    "/etc",
    "/tmp",
    "/dev/urandom",
];
const WRITE_GRANTS: &[&str] = &["/tmp", "/dev/null"];

/// Build the sandlock argv wrapping `args` with the default policy:
/// `[sandlock, run, <policy flags>, --, args...]`.
///
/// `exists` probes path presence in the guest rootfs (production passes
/// `Path::exists`); it also guards the sandlock binary itself, probing
/// `SANDLOCK_PATHS` in order and using the first hit as argv[0]. Returns
/// Err when sandlock is not installed at any candidate — callers must
/// surface this as a hard error, never fall back to unsandboxed execution.
pub fn wrap_for_sandbox(
    args: &[String],
    work_dir: &str,
    exists: impl Fn(&str) -> bool,
) -> Result<Vec<String>, String> {
    let sandlock = SANDLOCK_PATHS.iter().find(|p| exists(p)).ok_or_else(|| {
        format!(
            "sandbox requested but sandlock not present in image (probed {})",
            SANDLOCK_PATHS.join(", ")
        )
    })?;
    let mut argv = vec![sandlock.to_string(), "run".to_string()];
    for p in READ_GRANTS {
        if exists(p) {
            argv.push("-r".into());
            argv.push((*p).into());
        }
    }
    if exists(work_dir) {
        argv.push("-w".into());
        argv.push(work_dir.to_string());
    }
    for p in WRITE_GRANTS {
        if exists(p) {
            argv.push("-w".into());
            argv.push((*p).into());
        }
    }
    argv.push("--".into());
    argv.extend(args.iter().cloned());
    Ok(argv)
}

/// Spawn `program` and capture its output. When `exec_id` is set the child
/// pid is registered under that id (see `crate::registry`) so a concurrent
/// `kill` command can killpg the process group; the registration is removed
/// when the exec returns, on every exit path.
pub fn exec_isolated(
    program: &str,
    args: &[String],
    work_dir: &str,
    timeout_secs: u64,
    exec_id: Option<&str>,
) -> Result<ExecResult, String> {
    if let Some(id) = exec_id {
        crate::registry::validate_exec_id(id)?;
    }
    let mut child = Command::new(program);
    // Agents inherit an almost-empty environment from init; give commands
    // a sane default PATH so /sbin tools (ip, apk, ...) resolve.
    child.env(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/sbin:/usr/sbin:/bin:/usr/bin".into()),
    );
    let mut child = child
        .args(&args[1..])
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn failed: {}", e))?;

    let pid = child.id();

    // Register under exec_id so a `kill` command can find this process
    // group. Duplicate id → kill the just-spawned child and fail honestly.
    // The guard unregisters on every return path below.
    let _guard = match exec_id {
        Some(id) => {
            if let Err(e) = crate::registry::register(id, pid as i32) {
                // SAFETY: pid is a valid process ID from Command::spawn()
                // just above; killpg(-pid, SIGKILL) kills the whole group.
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Err(e);
            }
            Some(crate::registry::UnregisterGuard::new(id))
        }
        None => None,
    };

    // Take pipes and spawn reader threads BEFORE waiting — avoids
    // pipe-buffer deadlock when child output exceeds 64KB.
    let stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();

    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        stdout_pipe
            .take(MAX_OUTPUT_BYTES as u64)
            .read_to_end(&mut buf)
            .ok();
        let _ = stdout_tx.send(buf);
    });
    thread::spawn(move || {
        let mut buf = Vec::new();
        stderr_pipe
            .take(MAX_OUTPUT_BYTES as u64)
            .read_to_end(&mut buf)
            .ok();
        let _ = stderr_tx.send(buf);
    });

    // Wait for child with timeout. Kill by pid on timeout.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait());
    });

    let exit_status = match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("wait failed: {}", e)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // SAFETY: pid is a valid process ID from Command::spawn().
            // killpg(-pid, SIGKILL) kills the entire process group,
            // preventing orphaned grandchild processes.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            return Err(format!("command timed out after {}s", timeout_secs));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("process wait thread panicked".into());
        }
    };

    let stdout_buf = stdout_rx.recv().unwrap_or_default();
    let stderr_buf = stderr_rx.recv().unwrap_or_default();

    if stdout_buf.len() >= MAX_OUTPUT_BYTES || stderr_buf.len() >= MAX_OUTPUT_BYTES {
        return Err("output exceeded 10 MB limit".into());
    }

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        exit_code: exit_status.code().unwrap_or(-1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        vec!["sh".into(), "-c".into(), "echo hi".into()]
    }

    /// All candidate paths present → full default policy.
    #[test]
    fn full_policy_when_everything_exists() {
        let argv = wrap_for_sandbox(&args(), "/workdir", |_| true).unwrap();
        assert_eq!(
            argv,
            vec![
                "/usr/bin/sandlock",
                "run",
                "-r",
                "/usr",
                "-r",
                "/lib",
                "-r",
                "/lib64",
                "-r",
                "/bin",
                "-r",
                "/sbin",
                "-r",
                "/etc",
                "-r",
                "/tmp",
                "-r",
                "/dev/urandom",
                "-w",
                "/workdir",
                "-w",
                "/tmp",
                "-w",
                "/dev/null",
                "--",
                "sh",
                "-c",
                "echo hi",
            ]
        );
    }

    /// busybox/alpine has no /lib64 or /sbin — those grants must be dropped
    /// so sandlock doesn't error on nonexistent paths.
    #[test]
    fn nonexistent_paths_are_filtered() {
        let missing = |p: &str| !matches!(p, "/lib64" | "/sbin");
        let argv = wrap_for_sandbox(&args(), "/workdir", missing).unwrap();
        assert!(!argv.iter().any(|a| a == "/lib64"));
        assert!(!argv.iter().any(|a| a == "/sbin"));
        assert!(argv.iter().any(|a| a == "/lib"));
        // -- separator still directly precedes the command.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[sep + 1..], args()[..]);
    }

    /// The session work_dir is the only non-/tmp read-write grant; "/"
    /// must never be granted (would leak sibling sessions under /workdir).
    #[test]
    fn workdir_writable_root_never_granted() {
        let argv = wrap_for_sandbox(&args(), "/workdir/session-1", |_| true).unwrap();
        let w = argv.iter().position(|a| a == "/workdir/session-1").unwrap();
        assert_eq!(argv[w - 1], "-w");
        assert!(!argv.iter().any(|a| a == "/"));
    }

    /// Missing sandlock binary → hard error naming both probed paths,
    /// no silent fallback.
    #[test]
    fn missing_sandlock_binary_is_an_error() {
        let exists = |p: &str| !SANDLOCK_PATHS.contains(&p);
        let err = wrap_for_sandbox(&args(), "/workdir", exists).unwrap_err();
        assert!(err.contains("/usr/bin/sandlock"));
        assert!(err.contains("/workdir/usr/bin/sandlock"));
    }

    /// Pool/hot-plug VMs boot from the busybox initramfs with the composed
    /// layers mounted at /workdir — sandlock only exists under /workdir,
    /// and that path becomes argv[0]. System-path grants that don't exist
    /// in the initramfs (e.g. /usr) are filtered as usual.
    #[test]
    fn pool_mode_uses_workdir_sandlock() {
        let exists = |p: &str| {
            p == "/workdir/usr/bin/sandlock" || p == "/tmp" || p.starts_with("/workdir/session")
        };
        let argv = wrap_for_sandbox(&args(), "/workdir/session-1", exists).unwrap();
        assert_eq!(argv[0], "/workdir/usr/bin/sandlock");
        // Initramfs has no /usr, /lib, ... so those grants are filtered.
        assert!(!argv.iter().any(|a| a == "/usr"));
        // The mounted workdir is still writable.
        let w = argv.iter().position(|a| a == "/workdir/session-1").unwrap();
        assert_eq!(argv[w - 1], "-w");
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[sep + 1..], args()[..]);
    }

    /// When both candidates exist (cold boot), the first one wins.
    #[test]
    fn cold_boot_path_wins_when_both_exist() {
        let argv = wrap_for_sandbox(&args(), "/workdir", |_| true).unwrap();
        assert_eq!(argv[0], "/usr/bin/sandlock");
    }
}

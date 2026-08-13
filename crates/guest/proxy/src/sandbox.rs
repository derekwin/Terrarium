//! Sandbox execution: spawn and capture process output.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use adapter_traits::{
    Capability, DefaultAccess, Direction, FileAccess, PathPattern, SandboxPolicy,
    SANDBOX_DENY_EXIT_CODE,
};

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// Structured policy-denial signal (M7): true only when the sandlock
    /// supervisor itself reported a denied syscall on the deny channel —
    /// never inferred from child stderr text.
    pub denied: bool,
}

const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

/// fd the sandlock supervisor writes deny records to (inherited via
/// `SANDBOX_DENY_FD`, CLOEXEC cleared; see the sandlock denyfd patch).
/// A high number avoids colliding with the stdio/pipe fds std::process
/// allocates for the child.
const DENY_FD: i32 = 63;

/// In-guest candidates for the sandlock confinement binary, probed in
/// order; the first that exists wins. Cold-boot VMs have composed layers
/// as the root fs ("/usr/bin/sandlock"); pool/hot-plug VMs boot from the
/// busybox initramfs with composed layers mounted at /workdir
/// ("/workdir/usr/bin/sandlock").
pub const SANDLOCK_PATHS: &[&str] = &["/usr/bin/sandlock", "/workdir/usr/bin/sandlock"];

/// Native backend (terra-confine) probe paths, mirroring sandlock's
/// cold-boot vs pool-boot split.
pub const NATIVE_PATHS: &[&str] = &["/usr/bin/terra-confine", "/workdir/usr/bin/terra-confine"];

/// Build the terra-confine argv wrapping `args`, translating the policy
/// into `[terra-confine, run, -r/-w grants, --net-allow, -m, -w workdir,
/// --, args...]`.
///
/// Semantics (native backend):
/// - Landlock fs: `-r` read-only grants, `-w` read-write grants; a path
///   not granted is denied by the kernel (zero per-syscall overhead).
/// - Network: `--net-allow host[:port]` whitelist, enforced by the
///   seccomp supervisor; **no flag means default-deny** (unlike sandlock,
///   no all-deny injection is needed).
/// - `limits.memory_mb` → `-m <n>M` (cgroup v2 memory.max).
/// - `limits.procs` is not enforceable by terra-confine v1 (no cgroup
///   pids controller in the guest kernel); it is intentionally ignored.
/// - `File::Execute`, `Network::Inbound`, `Device` are rejected (the
///   engine already fails them at validate).
pub fn wrap_for_confine(
    args: &[String],
    work_dir: &str,
    policy: Option<&SandboxPolicy>,
    exists: impl Fn(&str) -> bool,
) -> Result<Vec<String>, String> {
    let ts = NATIVE_PATHS.iter().find(|p| exists(p)).ok_or_else(|| {
        format!(
            "sandbox requested but terra-confine not present in image (probed {})",
            NATIVE_PATHS.join(", ")
        )
    })?;
    let mut argv = vec![ts.to_string(), "run".to_string()];
    let mut wants_dev = false;

    if let Some(policy) = policy {
        if policy.default == DefaultAccess::Allow {
            return Err("DefaultAccess::Allow is not allowed".into());
        }
        for cap in &policy.capabilities {
            match cap {
                Capability::File { path, access } => {
                    let (flag, path_str) = match (path, access) {
                        (PathPattern::Prefix(p) | PathPattern::Exact(p), FileAccess::Read) => {
                            ("-r", p.to_string_lossy())
                        }
                        (PathPattern::Prefix(p) | PathPattern::Exact(p), FileAccess::ReadWrite) => {
                            ("-w", p.to_string_lossy())
                        }
                        (_, FileAccess::Execute) => {
                            return Err("Execute capability not supported by this backend".into());
                        }
                    };
                    let is_dev = path_str.starts_with("/dev/");
                    if exists(&path_str) {
                        argv.push(flag.into());
                        argv.push(path_str.into_owned());
                        // Landlock grants are directory-scoped — a device
                        // grant like /dev/urandom or /dev/null cannot be
                        // expressed exactly. When the policy asks for any
                        // /dev device, widen to a read-only /dev (the guest
                        // kernel has STRICT_DEVMEM, so /dev/mem etc. stay
                        // kernel-restricted; block devices don't exist in
                        // the virtiofs guest).
                        if is_dev {
                            wants_dev = true;
                        }
                    }
                }
                Capability::Network {
                    endpoint,
                    direction,
                } => match direction {
                    Direction::Outbound => {
                        let entry = match endpoint.port {
                            Some(port) => format!("{}:{}", endpoint.host, port),
                            None => endpoint.host.clone(),
                        };
                        argv.push("--net-allow".into());
                        argv.push(entry);
                    }
                    Direction::Inbound => {
                        return Err(
                            "Inbound network capability not supported by this backend".into()
                        );
                    }
                },
                Capability::Device { .. } => {
                    return Err("Device capability not supported by this backend".into());
                }
            }
        }
        if let Some(mb) = policy.limits.memory_mb {
            argv.push("-m".into());
            argv.push(format!("{}M", mb));
        }
    }

    if wants_dev && exists("/dev") {
        argv.push("-r".into());
        argv.push("/dev".into());
    }

    if exists(work_dir) {
        argv.push("-w".into());
        argv.push(work_dir.to_string());
    }

    argv.push("--".into());
    argv.extend(args.iter().cloned());
    Ok(argv)
}

/// Build the sandlock argv wrapping `args`, translating the policy's
/// capability set into sandlock flags (D3), i.e.
/// `[sandlock, run, <policy flags>, <limits>, -w work_dir, --, args...]`.
///
/// The policy type is `adapter_traits::SandboxPolicy` — the single policy
/// type shared with the engine, which injects the full policy (default or
/// user) on the wire for every sandboxed exec (D2). This guest applies NO
/// implicit defaults: the granted paths are exactly the policy's
/// capabilities plus the dynamic session workdir.
///
/// Capability → sandlock flag:
/// - `File { Read }` (Prefix or Exact)     → `-r <path>`  (exists-filtered)
/// - `File { ReadWrite }` (Prefix or Exact)→ `-w <path>`  (exists-filtered)
/// - `Network { Outbound }`                → `--net-allow <host>[:<port>]`
/// - `limits.memory_mb`                    → `-m <n>M`
/// - `limits.procs`                        → `-P <n>`
/// - `limits.cpu_shares` / `fds` / `bandwidth_kbps` are not expressible in
///   sandlock and are intentionally ignored.
///
/// Honest-unsupported (D4): `File { Execute }`, `Network { Inbound }` and
/// `Device` capabilities return Err instead of being silently dropped.
///
/// `exists` probes path presence in the guest rootfs (production passes
/// `Path::exists`); it also guards the sandlock binary itself, probing
/// `SANDLOCK_PATHS` in order and using the first hit as argv[0]. Returns
/// Err when sandlock is not installed at any candidate — callers must
/// surface this as a hard error, never fall back to unsandboxed execution.
pub fn wrap_for_sandbox(
    args: &[String],
    work_dir: &str,
    policy: Option<&SandboxPolicy>,
    exists: impl Fn(&str) -> bool,
) -> Result<Vec<String>, String> {
    let sandlock = SANDLOCK_PATHS.iter().find(|p| exists(p)).ok_or_else(|| {
        format!(
            "sandbox requested but sandlock not present in image (probed {})",
            SANDLOCK_PATHS.join(", ")
        )
    })?;
    let mut argv = vec![sandlock.to_string(), "run".to_string()];

    if let Some(policy) = policy {
        // Defense in depth: the engine already rejects Allow (D6), but an
        // Allow-capable default would silently widen every grant below.
        if policy.default == DefaultAccess::Allow {
            return Err("DefaultAccess::Allow is not allowed".into());
        }
        for cap in &policy.capabilities {
            match cap {
                Capability::File { path, access } => {
                    let (flag, path_str) = match (path, access) {
                        (PathPattern::Prefix(p) | PathPattern::Exact(p), FileAccess::Read) => {
                            ("-r", p.to_string_lossy())
                        }
                        (PathPattern::Prefix(p) | PathPattern::Exact(p), FileAccess::ReadWrite) => {
                            ("-w", p.to_string_lossy())
                        }
                        (_, FileAccess::Execute) => {
                            return Err("Execute capability not supported by this backend".into());
                        }
                    };
                    // Grant only paths that exist — sandlock errors on
                    // nonexistent grant paths.
                    if exists(&path_str) {
                        argv.push(flag.into());
                        argv.push(path_str.into_owned());
                    }
                }
                Capability::Network {
                    endpoint,
                    direction,
                } => match direction {
                    Direction::Outbound => {
                        let entry = match endpoint.port {
                            Some(port) => format!("{}:{}", endpoint.host, port),
                            None => endpoint.host.clone(),
                        };
                        argv.push("--net-allow".into());
                        argv.push(entry);
                    }
                    Direction::Inbound => {
                        return Err(
                            "Inbound network capability not supported by this backend".into()
                        );
                    }
                },
                Capability::Device { .. } => {
                    return Err("Device capability not supported by this backend".into());
                }
            }
        }
        if let Some(mb) = policy.limits.memory_mb {
            argv.push("-m".into());
            argv.push(format!("{}M", mb));
        }
        if let Some(procs) = policy.limits.procs {
            argv.push("-P".into());
            argv.push(procs.to_string());
        }
    }

    // Default-deny networking: sandlock only supervises the network when
    // net rules are present, and a missing rule list means "allow all".
    // The policy model's default is deny, so a sandboxed exec with no
    // explicit Network::Outbound grants must get an all-deny net policy
    // (--net-deny is mutually exclusive with --net-allow, so this only
    // runs when no allow rules were emitted above).
    if !argv.iter().any(|a| a == "--net-allow") {
        argv.push("--net-deny".into());
        argv.push("0.0.0.0/0".into());
        argv.push("--net-deny".into());
        argv.push("::/0".into());
    }

    // The session workdir is a dynamic grant — it travels via the exec
    // `work_dir` field, never as a static capability.
    if exists(work_dir) {
        argv.push("-w".into());
        argv.push(work_dir.to_string());
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
    deny_signal: bool,
) -> Result<ExecResult, String> {
    if let Some(id) = exec_id {
        crate::registry::validate_exec_id(id)?;
    }

    // Structured deny channel (M7): when confining with sandlock, hand it
    // a pipe via SANDBOX_DENY_FD (fd 63, CLOEXEC cleared) and classify
    // denials from what sandlock itself writes — never from child stderr
    // text. Unsandboxed execs skip the pipe entirely.
    let (deny_read, deny_write) = if deny_signal {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(format!(
                "failed to create deny pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        (fds[0], fds[1])
    } else {
        (-1, -1)
    };

    let mut child = Command::new(program);
    // Agents inherit an almost-empty environment from init; give commands
    // a sane default PATH so /sbin tools (ip, apk, ...) resolve.
    child.env(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/sbin:/usr/sbin:/bin:/usr/bin".into()),
    );
    if deny_write >= 0 {
        child.env("SANDBOX_DENY_FD", DENY_FD.to_string());
        // SAFETY: dup2/fcntl are async-signal-safe and only touch this
        // process's own fds inside the pre-exec child.
        unsafe {
            child.pre_exec(move || {
                if libc::dup2(deny_write, DENY_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let flags = libc::fcntl(DENY_FD, libc::F_GETFD);
                if flags < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(DENY_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = match child
        .args(&args[1..])
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            close_fd_pair(deny_read, deny_write);
            return Err(format!("spawn failed: {}", e));
        }
    };
    // The child owns the write end now — close ours so EOF arrives the
    // moment the sandlock supervisor exits.
    if deny_write >= 0 {
        unsafe { libc::close(deny_write) };
    }

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
                drain_and_close_deny(deny_read);
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
        Ok(Err(e)) => {
            drain_and_close_deny(deny_read);
            return Err(format!("wait failed: {}", e));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // SAFETY: pid is a valid process ID from Command::spawn().
            // killpg(-pid, SIGKILL) kills the entire process group,
            // preventing orphaned grandchild processes.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            drain_and_close_deny(deny_read);
            return Err(format!("command timed out after {}s", timeout_secs));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            drain_and_close_deny(deny_read);
            return Err("process wait thread panicked".into());
        }
    };

    let denied = drain_and_close_deny(deny_read);
    let stdout_buf = stdout_rx.recv().unwrap_or_default();
    let stderr_buf = stderr_rx.recv().unwrap_or_default();

    if stdout_buf.len() >= MAX_OUTPUT_BYTES || stderr_buf.len() >= MAX_OUTPUT_BYTES {
        return Err("output exceeded 10 MB limit".into());
    }

    Ok(ExecResult {
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        exit_code: exit_status.code().unwrap_or(-1),
        denied,
    })
}

/// Map a sandlock policy denial onto the structured deny signal (M7).
///
/// The signal is `ExecResult.denied` — set only when the sandlock
/// supervisor itself reported a denied syscall through the deny channel
/// (see [`exec_isolated`] and the sandlock denyfd patch). The exit code
/// is rewritten to `adapter_traits::SANDBOX_DENY_EXIT_CODE` when a denial
/// was reported AND the exec failed: a denied attempt the command
/// recovered from (exit 0) is not a rejected exec. Child stderr text is
/// NEVER a signal — it is carried only as the informative reason.
///
/// Callers must apply this ONLY to sandboxed execs — a legitimate exit
/// 200 from an unsandboxed command must never be misclassified.
pub fn classify_sandlock_result(mut result: ExecResult) -> ExecResult {
    if result.denied && result.exit_code != 0 {
        result.exit_code = SANDBOX_DENY_EXIT_CODE;
    }
    result
}

/// Close both ends of a deny pipe (best-effort).
fn close_fd_pair(read_fd: libc::c_int, write_fd: libc::c_int) {
    if read_fd >= 0 {
        unsafe { libc::close(read_fd) };
    }
    if write_fd >= 0 {
        unsafe { libc::close(write_fd) };
    }
}

/// Drain the deny pipe to EOF and report whether the sandlock supervisor
/// recorded any denial. Best-effort: read errors are treated as
/// end-of-stream; the fd is always closed.
fn drain_and_close_deny(read_fd: libc::c_int) -> bool {
    if read_fd < 0 {
        return false;
    }
    let mut buf = [0u8; 4096];
    let mut saw_record = false;
    loop {
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break; // 0 = EOF; <0 = error (best-effort, treat as EOF)
        }
        saw_record = true;
    }
    unsafe { libc::close(read_fd) };
    saw_record
}

#[cfg(test)]
mod tests {
    use super::*;
    use adapter_traits::Endpoint;

    fn args() -> Vec<String> {
        vec!["sh".into(), "-c".into(), "echo hi".into()]
    }

    /// Mirror of the engine's `default_sandbox_policy()` — the engine
    /// injects this on the wire for every sandboxed exec; these tests use
    /// the same capability set.
    fn default_policy() -> SandboxPolicy {
        SandboxPolicy {
            capabilities: vec![
                Capability::File {
                    path: PathPattern::Prefix("/usr".into()),
                    access: FileAccess::Read,
                },
                Capability::File {
                    path: PathPattern::Prefix("/lib".into()),
                    access: FileAccess::Read,
                },
                Capability::File {
                    path: PathPattern::Prefix("/lib64".into()),
                    access: FileAccess::Read,
                },
                Capability::File {
                    path: PathPattern::Prefix("/bin".into()),
                    access: FileAccess::Read,
                },
                Capability::File {
                    path: PathPattern::Prefix("/sbin".into()),
                    access: FileAccess::Read,
                },
                Capability::File {
                    path: PathPattern::Prefix("/etc".into()),
                    access: FileAccess::Read,
                },
                Capability::File {
                    path: PathPattern::Prefix("/tmp".into()),
                    access: FileAccess::ReadWrite,
                },
                Capability::File {
                    path: PathPattern::Exact("/dev/null".into()),
                    access: FileAccess::ReadWrite,
                },
                Capability::File {
                    path: PathPattern::Exact("/dev/urandom".into()),
                    access: FileAccess::Read,
                },
            ],
            limits: Default::default(),
            default: DefaultAccess::Deny,
            audit: Default::default(),
            version: 1,
        }
    }

    /// All candidate paths present → full policy translated to flags.
    #[test]
    fn full_policy_when_everything_exists() {
        let argv =
            wrap_for_sandbox(&args(), "/workdir", Some(&default_policy()), |_| true).unwrap();
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
                "-w",
                "/tmp",
                "-w",
                "/dev/null",
                "-r",
                "/dev/urandom",
                "--net-deny",
                "0.0.0.0/0",
                "--net-deny",
                "::/0",
                "-w",
                "/workdir",
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
        let argv = wrap_for_sandbox(&args(), "/workdir", Some(&default_policy()), missing).unwrap();
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
        let argv = wrap_for_sandbox(
            &args(),
            "/workdir/session-1",
            Some(&default_policy()),
            |_| true,
        )
        .unwrap();
        let w = argv.iter().position(|a| a == "/workdir/session-1").unwrap();
        assert_eq!(argv[w - 1], "-w");
        assert!(!argv.iter().any(|a| a == "/"));
    }

    /// Missing sandlock binary → hard error naming both probed paths,
    /// no silent fallback.
    #[test]
    fn missing_sandlock_binary_is_an_error() {
        let exists = |p: &str| !SANDLOCK_PATHS.contains(&p);
        let err =
            wrap_for_sandbox(&args(), "/workdir", Some(&default_policy()), exists).unwrap_err();
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
        let argv = wrap_for_sandbox(
            &args(),
            "/workdir/session-1",
            Some(&default_policy()),
            exists,
        )
        .unwrap();
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
        let argv =
            wrap_for_sandbox(&args(), "/workdir", Some(&default_policy()), |_| true).unwrap();
        assert_eq!(argv[0], "/usr/bin/sandlock");
    }

    /// Capabilities are translated in order; a capability on a nonexistent
    /// path is filtered like the system grants.
    #[test]
    fn user_capabilities_follow_defaults_and_are_filtered() {
        let mut policy = default_policy();
        policy.capabilities.push(Capability::File {
            path: PathPattern::Exact("/opt/data".into()),
            access: FileAccess::Read,
        });
        policy.capabilities.push(Capability::File {
            path: PathPattern::Exact("/missing".into()),
            access: FileAccess::Read,
        });
        policy.capabilities.push(Capability::File {
            path: PathPattern::Exact("/output".into()),
            access: FileAccess::ReadWrite,
        });
        let exists = |p: &str| p != "/missing";
        let argv = wrap_for_sandbox(&args(), "/workdir", Some(&policy), exists).unwrap();
        // Defaults still there.
        assert!(argv.iter().any(|a| a == "/usr"));
        // User grants translated with the right flags.
        let r = argv.iter().position(|a| a == "/opt/data").unwrap();
        assert_eq!(argv[r - 1], "-r");
        let w = argv.iter().position(|a| a == "/output").unwrap();
        assert_eq!(argv[w - 1], "-w");
        // Nonexistent user grant is filtered like the defaults.
        assert!(!argv.iter().any(|a| a == "/missing"));
        // User grants come after the defaults, before "--".
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert!(r < sep && w < sep);
        let last_default = argv.iter().position(|a| a == "/dev/urandom").unwrap();
        assert!(r > last_default && w > last_default);
    }

    /// Outbound network capabilities emit `--net-allow <host>[:<port>]`;
    /// a missing port emits the bare host.
    #[test]
    fn network_outbound_emits_net_allow() {
        let policy = SandboxPolicy {
            capabilities: vec![
                Capability::Network {
                    endpoint: Endpoint {
                        host: "api.openai.com".into(),
                        port: Some(443),
                    },
                    direction: Direction::Outbound,
                },
                Capability::Network {
                    endpoint: Endpoint {
                        host: "pypi.org".into(),
                        port: None,
                    },
                    direction: Direction::Outbound,
                },
            ],
            ..default_policy()
        };
        let argv = wrap_for_sandbox(&args(), "/workdir", Some(&policy), |_| true).unwrap();
        let a = argv.iter().position(|x| x == "api.openai.com:443").unwrap();
        assert_eq!(argv[a - 1], "--net-allow");
        let b = argv.iter().position(|x| x == "pypi.org").unwrap();
        assert_eq!(argv[b - 1], "--net-allow");
    }

    /// No network capabilities → no --net-allow flags, and an all-deny
    /// net policy is injected (default-deny semantics: sandlock only
    /// supervises networking when rules are present, so without rules it
    /// would otherwise mean "allow all").
    #[test]
    fn no_network_caps_means_default_deny_net() {
        let argv =
            wrap_for_sandbox(&args(), "/workdir", Some(&default_policy()), |_| true).unwrap();
        assert!(!argv.iter().any(|a| a == "--net-allow"));
        assert!(argv.iter().any(|a| a == "--net-deny"));
        assert!(argv.iter().any(|a| a == "0.0.0.0/0"));
        assert!(argv.iter().any(|a| a == "::/0"));
    }

    /// Explicit Network::Outbound grants suppress the all-deny injection
    /// (--net-deny is mutually exclusive with --net-allow in sandlock).
    #[test]
    fn explicit_network_capability_suppresses_default_deny() {
        let policy = SandboxPolicy {
            capabilities: vec![Capability::Network {
                endpoint: Endpoint {
                    host: "api.openai.com".into(),
                    port: Some(443),
                },
                direction: Direction::Outbound,
            }],
            ..default_policy()
        };
        let argv = wrap_for_sandbox(&args(), "/workdir", Some(&policy), |_| true).unwrap();
        assert!(argv.iter().any(|a| a == "--net-allow"));
        assert!(!argv.iter().any(|a| a == "--net-deny"));
    }

    /// memory_mb → "-m <n>M"; procs → "-P <n>", both before "--".
    #[test]
    fn policy_memory_and_procs_formatting() {
        let mut policy = default_policy();
        policy.limits.memory_mb = Some(512);
        policy.limits.procs = Some(20);
        let argv = wrap_for_sandbox(&args(), "/workdir", Some(&policy), |_| true).unwrap();
        let m = argv.iter().position(|a| a == "-m").unwrap();
        assert_eq!(argv[m + 1], "512M");
        let p = argv.iter().position(|a| a == "-P").unwrap();
        assert_eq!(argv[p + 1], "20");
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert!(m < sep && p < sep, "limits must precede the -- separator");
    }

    /// cpu_shares / fds / bandwidth_kbps have no sandlock flag — they are
    /// ignored and the argv is identical to a policy without them.
    #[test]
    fn unsupported_limits_are_ignored() {
        let mut policy = default_policy();
        policy.limits.cpu_shares = Some(100);
        policy.limits.fds = Some(64);
        policy.limits.bandwidth_kbps = Some(1024);
        let argv = wrap_for_sandbox(&args(), "/workdir", Some(&policy), |_| true).unwrap();
        let base =
            wrap_for_sandbox(&args(), "/workdir", Some(&default_policy()), |_| true).unwrap();
        assert_eq!(argv, base);
    }

    /// `File::Execute` is a contract-bearing capability sandlock cannot
    /// express — the translation fails honestly instead of silently
    /// downgrading to Read.
    #[test]
    fn execute_capability_is_unsupported() {
        let mut policy = default_policy();
        policy.capabilities.push(Capability::File {
            path: PathPattern::Exact("/usr/bin/foo".into()),
            access: FileAccess::Execute,
        });
        let err = wrap_for_sandbox(&args(), "/workdir", Some(&policy), |_| true).unwrap_err();
        assert!(err.contains("Execute capability not supported"), "{}", err);
    }

    /// `Network::Inbound` cannot be expressed by sandlock — honest error.
    #[test]
    fn inbound_network_capability_is_unsupported() {
        let policy = SandboxPolicy {
            capabilities: vec![Capability::Network {
                endpoint: Endpoint {
                    host: "0.0.0.0".into(),
                    port: Some(8080),
                },
                direction: Direction::Inbound,
            }],
            ..default_policy()
        };
        let err = wrap_for_sandbox(&args(), "/workdir", Some(&policy), |_| true).unwrap_err();
        assert!(
            err.contains("Inbound network capability not supported"),
            "{}",
            err
        );
    }

    /// `Device` capabilities cannot be expressed by sandlock — honest error.
    #[test]
    fn device_capability_is_unsupported() {
        let policy = SandboxPolicy {
            capabilities: vec![Capability::Device {
                path: "/dev/kvm".into(),
            }],
            ..default_policy()
        };
        let err = wrap_for_sandbox(&args(), "/workdir", Some(&policy), |_| true).unwrap_err();
        assert!(err.contains("Device capability not supported"), "{}", err);
    }

    /// `DefaultAccess::Allow` is a debug escape hatch — defense in depth
    /// even though the engine rejects it before sending.
    #[test]
    fn default_access_allow_is_rejected() {
        let policy = SandboxPolicy {
            default: DefaultAccess::Allow,
            ..default_policy()
        };
        let err = wrap_for_sandbox(&args(), "/workdir", Some(&policy), |_| true).unwrap_err();
        assert!(
            err.contains("DefaultAccess::Allow is not allowed"),
            "{}",
            err
        );
    }

    /// No policy on the wire (direct protocol client): the guest grants
    /// nothing beyond the session workdir — no implicit defaults.
    #[test]
    fn no_policy_grants_only_workdir() {
        let argv = wrap_for_sandbox(&args(), "/workdir/session-1", None, |_| true).unwrap();
        assert_eq!(
            argv,
            vec![
                "/usr/bin/sandlock",
                "run",
                "--net-deny",
                "0.0.0.0/0",
                "--net-deny",
                "::/0",
                "-w",
                "/workdir/session-1",
                "--",
                "sh",
                "-c",
                "echo hi",
            ]
        );
    }

    /// The policy object rejects unknown fields (deny_unknown_fields on
    /// `adapter_traits::SandboxPolicy`) and the pinned wire shape parses.
    #[test]
    fn policy_deserialize_rejects_unknown_fields() {
        let res: Result<SandboxPolicy, _> = serde_json::from_str(
            r#"{"capabilities":[{"File":{"path":{"Exact":"/x"},"access":"Read"}}],"bogus":1}"#,
        );
        assert!(res.is_err());
        // ...and the pinned wire shape (capabilities/limits/default/version)
        // parses into the shared type.
        let policy: SandboxPolicy = serde_json::from_str(
            r#"{
                "capabilities": [
                    {"File": {"path": {"Prefix": "/usr"}, "access": "Read"}},
                    {"File": {"path": {"Exact": "/dev/null"}, "access": "ReadWrite"}},
                    {"Network": {"endpoint": {"host": "api.openai.com", "port": 443},
                                 "direction": "Outbound"}}
                ],
                "limits": {"memory_mb": 512, "procs": 20},
                "default": "deny",
                "version": 1
            }"#,
        )
        .unwrap();
        assert_eq!(policy.capabilities.len(), 3);
        assert_eq!(policy.limits.memory_mb, Some(512));
        assert_eq!(policy.limits.procs, Some(20));
        assert_eq!(policy.default, DefaultAccess::Deny);
    }

    /// A deny record from the sandlock supervisor + nonzero exit → the
    /// exit code is rewritten to the reserved deny code (M7). Child
    /// stderr is carried only as the informative reason.
    #[test]
    fn supervisor_deny_record_maps_to_deny_exit_code() {
        let result = classify_sandlock_result(ExecResult {
            stdout: String::new(),
            stderr: "cat: /etc/passwd: Permission denied".into(),
            exit_code: 1,
            denied: true,
        });
        assert_eq!(result.exit_code, adapter_traits::SANDBOX_DENY_EXIT_CODE);
        assert_eq!(result.stderr, "cat: /etc/passwd: Permission denied");
    }

    /// No deny record → never a deny, even when stderr contains the word
    /// "denied" — the pre-M7 fuzzy sniffing misclassified this.
    #[test]
    fn denied_stderr_without_deny_record_passes_through() {
        let result = classify_sandlock_result(ExecResult {
            stdout: String::new(),
            stderr: "echo: denied".into(),
            exit_code: 3,
            denied: false,
        });
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.stderr, "echo: denied");
    }

    /// A deny record with a successful exec is not a rejected exec.
    #[test]
    fn deny_record_with_zero_exit_passes_through() {
        let result = classify_sandlock_result(ExecResult {
            stdout: "hi\n".into(),
            stderr: String::new(),
            exit_code: 0,
            denied: true,
        });
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "hi\n");
    }

    /// A plain nonzero exit passes through unchanged.
    #[test]
    fn nonzero_without_deny_record_passes_through() {
        let result = classify_sandlock_result(ExecResult {
            stdout: String::new(),
            stderr: "command not found".into(),
            exit_code: 127,
            denied: false,
        });
        assert_eq!(result.exit_code, 127);
        assert_eq!(result.stderr, "command not found");
    }

    /// The deny pipe drain detects a supervisor record.
    #[test]
    fn deny_pipe_drain_detects_records() {
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        let rec = b"{\"syscall\":\"openat\",\"errno\":13}\n";
        let written = unsafe { libc::write(w, rec.as_ptr() as *const libc::c_void, rec.len()) };
        assert_eq!(written as usize, rec.len());
        unsafe { libc::close(w) };
        assert!(drain_and_close_deny(r), "a record must be detected");
    }

    /// An empty deny pipe means no denial.
    #[test]
    fn deny_pipe_drain_empty_is_not_denied() {
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (r, w) = (fds[0], fds[1]);
        unsafe { libc::close(w) };
        assert!(!drain_and_close_deny(r), "no records → no deny");
    }
}

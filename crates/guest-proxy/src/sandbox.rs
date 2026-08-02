//! Sandbox execution: spawn and capture process output.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use adapter_traits::{
    Capability, DefaultAccess, Direction, FileAccess, PathPattern, SandboxPolicy,
};

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

/// Map a sandlock policy denial onto the structured deny signal (M4).
///
/// sandlock rejects denied accesses by printing a "denied" marker to
/// stderr and exiting nonzero. Sniffing that text in the engine is
/// fragile — a wording change in the pinned binary would silently kill
/// the deny audit. This module owns the sandlock integration, so it owns
/// the marker check: on a denial the exit code is rewritten to
/// `adapter_traits::SANDBOX_DENY_EXIT_CODE`, which travels the wire and is
/// what consumers match on. Everything else passes through unchanged.
///
/// Callers must apply this ONLY to sandboxed execs — a legitimate exit
/// 200 from an unsandboxed command must never be misclassified.
pub fn classify_sandlock_result(result: ExecResult) -> ExecResult {
    if result.exit_code != 0 && result.stderr.contains("denied") {
        ExecResult {
            exit_code: adapter_traits::SANDBOX_DENY_EXIT_CODE,
            ..result
        }
    } else {
        result
    }
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

    /// No network capabilities → no --net-allow flags.
    #[test]
    fn no_network_caps_means_no_net_allow_flags() {
        let argv =
            wrap_for_sandbox(&args(), "/workdir", Some(&default_policy()), |_| true).unwrap();
        assert!(!argv.iter().any(|a| a == "--net-allow"));
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

    /// A sandlock policy denial: nonzero child exit + the "denied" marker
    /// on stderr → the exit code is rewritten to the reserved deny code so
    /// the engine audits the deny structurally (M4), never by parsing
    /// stderr text.
    #[test]
    fn sandlock_denial_maps_to_deny_exit_code() {
        let result = classify_sandlock_result(ExecResult {
            stdout: String::new(),
            stderr: "sandlock: access denied for /etc/passwd".into(),
            exit_code: 1,
        });
        assert_eq!(result.exit_code, adapter_traits::SANDBOX_DENY_EXIT_CODE);
        // stdout/stderr pass through untouched — stderr stays the reason.
        assert_eq!(result.stderr, "sandlock: access denied for /etc/passwd");
    }

    /// A plain nonzero exit without the marker is NOT a denial — pass
    /// through unchanged (e.g. `sh -c "exit 127"`).
    #[test]
    fn nonzero_without_denial_marker_passes_through() {
        let result = classify_sandlock_result(ExecResult {
            stdout: String::new(),
            stderr: "command not found".into(),
            exit_code: 127,
        });
        assert_eq!(result.exit_code, 127);
        assert_eq!(result.stderr, "command not found");
    }

    /// A successful exec passes through unchanged.
    #[test]
    fn successful_exec_passes_through() {
        let result = classify_sandlock_result(ExecResult {
            stdout: "hi\n".into(),
            stderr: String::new(),
            exit_code: 0,
        });
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "hi\n");
    }
}

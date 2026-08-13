//! guest-proxy — host→guest command relay.
//!
//! Single transport: vsock port 1024 for the host (via CH `--vsock ... socket=...`).
//!
//! Executes commands locally and returns stdout/stderr/exit_code.
//! Commands with `"sandbox": true` are confined via sandlock
//! (Landlock/seccomp); plain exec remains a simple command forwarder.

mod registry;
mod sandbox;
mod vsock;

use std::io::{BufRead, BufReader, Read, Write};
use std::thread;
use std::time::Duration;

const VSOCK_PORT: u32 = 1024;

/// Whether a sysfs entry name is a CPU directory (`cpuN`, N a number).
fn is_cpu_dir(name: &str) -> bool {
    name.len() > 3 && name.starts_with("cpu") && name[3..].chars().all(|c| c.is_ascii_digit())
}

/// CPU hotplug helper: CH hot-adds vCPUs offline — the guest must
/// online them itself (writes to /sys/devices/system/cpu/cpuN/online).
/// Poll sysfs every 2s and online any CPU whose `online` file reads 0.
/// cpu0 has no `online` file on x86 — missing files are skipped.
/// Runs for the VM's lifetime; individual errors are ignored (a CPU
/// may be mid-removal) so the thread never panics the process.
fn start_cpu_onliner() {
    thread::spawn(|| loop {
        if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !is_cpu_dir(&name) {
                    continue;
                }
                let online = entry.path().join("online");
                let needs_online = std::fs::read_to_string(&online)
                    .map(|s| s.trim() == "0")
                    .unwrap_or(false);
                if needs_online && std::fs::write(&online, "1").is_ok() {
                    eprintln!("guest-proxy: onlined hot-added {}", name);
                }
            }
        }
        thread::sleep(Duration::from_secs(2));
    });
}

fn main() {
    start_cpu_onliner();

    // vsock listener for the host (FS-M4 hot-plug path). Optional: the
    // device may be absent (plain boots), then we just skip it. The
    // accept loop runs on the main thread so the process stays alive.
    match vsock::listen(VSOCK_PORT) {
        Ok(fd) => {
            eprintln!("guest-proxy: vsock listening on port {}", VSOCK_PORT);
            loop {
                match vsock::accept(fd) {
                    Ok(conn_fd) => {
                        thread::spawn(move || {
                            if let Ok(stream) = vsock::from_raw_fd_checked(conn_fd) {
                                handle(stream);
                            }
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        }
        Err(e) => {
            eprintln!("guest-proxy: vsock unavailable ({})", e);
        }
    }
}

fn handle<S: Read + Write>(mut stream: S) {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        if reader.read_line(&mut line).is_err() {
            return;
        }
    }

    let cmd: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(c) => c,
        Err(e) => {
            let resp =
                serde_json::json!({"status": "error", "message": format!("invalid json: {}", e)});
            let _ = writeln!(stream, "{}", resp);
            return;
        }
    };

    let command = cmd["command"].as_str().unwrap_or("");
    match command {
        "exec" => exec_cmd(&mut stream, &cmd),
        "kill" => kill_cmd(&mut stream, &cmd),
        "reset" => reset_cmd(&mut stream),
        "mount" => mount_cmd(&mut stream, &cmd, false),
        "umount" => mount_cmd(&mut stream, &cmd, true),
        "ping" => {
            let resp = serde_json::json!({"status": "ok", "message": "pong"});
            let _ = writeln!(stream, "{}", resp);
        }
        _ => {
            let resp = serde_json::json!({"status": "error", "message": format!("unknown command: {}", command)});
            let _ = writeln!(stream, "{}", resp);
        }
    }
}

/// {"command":"mount","tag":"<virtiofs tag>","target":"/workdir"}
/// {"command":"umount","target":"/workdir"}
fn mount_cmd<S: Read + Write>(stream: &mut S, cmd: &serde_json::Value, umount: bool) {
    let target = match cmd["target"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => {
            let resp = serde_json::json!({"status": "error", "message": "missing target"});
            let _ = writeln!(stream, "{}", resp);
            return;
        }
    };

    let result = if umount {
        std::process::Command::new("umount").arg(target).output()
    } else {
        let tag = cmd["tag"].as_str().unwrap_or("rootfs");
        let _ = std::fs::create_dir_all(target);
        std::process::Command::new("mount")
            .args(["-t", "virtiofs", tag, target])
            .output()
    };

    match result {
        Ok(out) if out.status.success() => {
            let resp = serde_json::json!({"status": "ok", "message": "ok"});
            let _ = writeln!(stream, "{}", resp);
        }
        Ok(out) => {
            let resp = serde_json::json!({
                "status": "error",
                "message": format!("mount failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
            });
            let _ = writeln!(stream, "{}", resp);
        }
        Err(e) => {
            let resp =
                serde_json::json!({"status": "error", "message": format!("spawn mount: {}", e)});
            let _ = writeln!(stream, "{}", resp);
        }
    }
}

/// {"command":"kill","exec_id":"<id registered by a live exec>"}
/// SIGKILLs the exec's process group; the blocked exec connection then
/// returns normally with the signal's exit code.
fn kill_cmd<S: Read + Write>(stream: &mut S, cmd: &serde_json::Value) {
    let exec_id = match cmd["exec_id"].as_str() {
        Some(id) => id,
        None => {
            let resp = serde_json::json!({"status": "error", "message": "missing exec_id"});
            let _ = writeln!(stream, "{}", resp);
            return;
        }
    };
    match registry::kill(exec_id) {
        Ok(()) => {
            let resp = serde_json::json!({"status": "ok", "message": "killed"});
            let _ = writeln!(stream, "{}", resp);
        }
        Err(e) => {
            let resp = serde_json::json!({"status": "error", "message": e});
            let _ = writeln!(stream, "{}", resp);
        }
    }
}

/// {"command":"reset"}
/// In-place episode reset (P1/RL fast path): SIGKILL every registered
/// exec process group and clear the guest's runtime tmpfs (/tmp, /run).
/// The host restores the overlay upper from the snapshot separately —
/// this command only handles the guest-side (processes + tmpfs) state.
fn reset_cmd<S: Read + Write>(stream: &mut S) {
    let killed = registry::kill_all();
    let _ = std::process::Command::new("sh")
        .args(["-c", "rm -rf /workdir/* /tmp/* /run/* 2>/dev/null"])
        .status();
    // Invalidate the guest's dentry/inode cache: the host replaces the
    // overlay upper during an in-place reset, and a cached dentry would
    // otherwise keep showing removed files (virtiofsd cache=always/auto
    // does not invalidate on host-side overlay changes).
    let _ = std::process::Command::new("sh")
        .args(["-c", "sync; echo 2 > /proc/sys/vm/drop_caches 2>/dev/null"])
        .status();
    let resp = serde_json::json!({"status": "ok", "killed": killed});
    let _ = writeln!(stream, "{}", resp);
}

fn exec_cmd<S: Read + Write>(stream: &mut S, cmd: &serde_json::Value) {
    let args: Vec<String> = match cmd["args"].as_array() {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => {
            let resp = serde_json::json!({"status": "error", "message": "missing args"});
            let _ = writeln!(stream, "{}", resp);
            return;
        }
    };
    if args.is_empty() {
        let resp = serde_json::json!({"status": "error", "message": "empty args"});
        let _ = writeln!(stream, "{}", resp);
        return;
    }

    // Default cwd: the mounted sandbox workspace when present.
    let work_dir = cmd["work_dir"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| {
            if std::path::Path::new("/workdir").is_dir() {
                "/workdir".into()
            } else {
                "/tmp".into()
            }
        });
    let work_dir = work_dir.as_str();
    let timeout = cmd["timeout_secs"].as_u64().unwrap_or(60).min(3600);
    let use_sandbox = cmd["sandbox"].as_bool().unwrap_or(false);

    // Optional per-exec sandlock policy. Only valid together with
    // "sandbox": true — a policy on an unsandboxed exec would silently
    // not apply, so reject it loudly instead. The engine injects the full
    // policy (default or user) for every sandboxed exec; the wire shape is
    // the shared adapter_traits::SandboxPolicy.
    let policy: Option<adapter_traits::SandboxPolicy> = match cmd.get("policy") {
        Some(p) if !p.is_null() => match serde_json::from_value(p.clone()) {
            Ok(p) => Some(p),
            Err(e) => {
                let resp = serde_json::json!({"status": "error", "message": format!("invalid policy: {}", e)});
                let _ = writeln!(stream, "{}", resp);
                return;
            }
        },
        // Absent, or an explicit JSON null (treated as absent).
        _ => None,
    };
    if policy.is_some() && !use_sandbox {
        let resp = serde_json::json!({"status": "error", "message": "policy requires sandboxed exec (set \"sandbox\": true)"});
        let _ = writeln!(stream, "{}", resp);
        return;
    }

    let exec_id = match cmd["exec_id"].as_str() {
        Some(id) => match registry::validate_exec_id(id) {
            Ok(()) => Some(id),
            Err(e) => {
                let resp = serde_json::json!({"status": "error", "message": e});
                let _ = writeln!(stream, "{}", resp);
                return;
            }
        },
        None => None,
    };

    let result = if use_sandbox {
        // Pick the L2 backend: explicit "native" / "sandlock" from the
        // adapter, else probe for terra-sandbox first (native is the
        // default backend). Hard error when the chosen binary is absent —
        // never silently fall back to unsandboxed execution.
        let exists = |p: &str| std::path::Path::new(p).exists();
        let native_present = sandbox::NATIVE_PATHS.iter().any(|p| exists(p));
        let wrapped = match cmd["backend"].as_str() {
            Some("sandlock") => {
                sandbox::wrap_for_sandbox(&args, work_dir, policy.as_ref(), &exists)
            }
            // native is the default backend; probe for it when no explicit
            // backend came over the wire (older engine ↔ new guest-proxy).
            Some("native") | None if native_present => {
                sandbox::wrap_for_native(&args, work_dir, policy.as_ref(), &exists)
            }
            Some("native") | None => {
                sandbox::wrap_for_sandbox(&args, work_dir, policy.as_ref(), &exists)
            }
            Some(other) => {
                let resp = serde_json::json!({"status": "error", "message": format!("unknown sandbox backend: {other}")});
                let _ = writeln!(stream, "{}", resp);
                return;
            }
        };
        match wrapped {
            Ok(argv) => sandbox::exec_isolated(&argv[0], &argv, work_dir, timeout, exec_id, true)
                .map(sandbox::classify_sandlock_result),
            Err(e) => Err(e),
        }
    } else {
        // Unsandboxed execs pass through untouched — a legitimate exit 200
        // must never be rewritten into the deny signal.
        sandbox::exec_isolated(&args[0], &args, work_dir, timeout, exec_id, false)
    };

    match result {
        Ok(o) => {
            let resp = serde_json::json!({
                "status": "ok",
                "message": "command executed",
                "data": {
                    "stdout": o.stdout,
                    "stderr": o.stderr,
                    "exit_code": o.exit_code,
                }
            });
            let _ = writeln!(stream, "{}", resp);
        }
        Err(e) => {
            let resp = serde_json::json!({"status": "error", "message": e});
            let _ = writeln!(stream, "{}", resp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run exec_cmd against an in-memory stream and parse the response.
    fn run_exec(cmd: serde_json::Value) -> serde_json::Value {
        let mut stream = std::io::Cursor::new(Vec::new());
        exec_cmd(&mut stream, &cmd);
        let out = String::from_utf8(stream.into_inner()).unwrap();
        serde_json::from_str(out.trim()).unwrap()
    }

    /// A policy on an unsandboxed exec would silently not apply — the
    /// exec_cmd layer must reject it loudly.
    #[test]
    fn policy_without_sandbox_is_rejected() {
        let resp = run_exec(serde_json::json!({
            "command": "exec",
            "args": ["echo", "hi"],
            "policy": {"capabilities": [{"File": {"path": {"Exact": "/opt/data"}, "access": "Read"}}]},
        }));
        assert_eq!(resp["status"], "error");
        assert!(
            resp["message"]
                .as_str()
                .unwrap()
                .contains("policy requires sandboxed exec"),
            "{:?}",
            resp
        );
    }

    /// Malformed policy objects are rejected before any exec happens.
    #[test]
    fn invalid_policy_shape_is_rejected() {
        let resp = run_exec(serde_json::json!({
            "command": "exec",
            "args": ["echo", "hi"],
            "sandbox": true,
            "policy": {"bogus": 1},
        }));
        assert_eq!(resp["status"], "error");
        assert!(
            resp["message"].as_str().unwrap().contains("invalid policy"),
            "{:?}",
            resp
        );
    }

    /// An explicit JSON null policy is treated as absent (no rejection).
    #[test]
    fn null_policy_is_absent() {
        let resp = run_exec(serde_json::json!({
            "command": "exec",
            "args": ["echo", "hi"],
            "policy": null,
        }));
        assert_eq!(resp["status"], "ok");
    }

    /// cpuN directory detection: real CPUs match, lookalikes don't.
    #[test]
    fn cpu_dir_detection() {
        assert!(is_cpu_dir("cpu0"));
        assert!(is_cpu_dir("cpu12"));
        assert!(!is_cpu_dir("cpu"));
        assert!(!is_cpu_dir("cpufreq"));
        assert!(!is_cpu_dir("cpuidle"));
        assert!(!is_cpu_dir("cpu1x"));
        assert!(!is_cpu_dir("other"));
    }
}

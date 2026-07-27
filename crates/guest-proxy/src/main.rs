//! guest-proxy — host→guest command relay.
//!
//! Two transports:
//! - Unix socket (/tmp/sandboxd.sock) for guest-local clients
//! - vsock port 1024 for the host (via CH `--vsock ... socket=...`)
//!
//! Executes commands locally and returns stdout/stderr/exit_code.
//! Not a sandbox — just a command forwarder.

mod sandbox;
mod vsock;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::thread;
use std::time::Duration;

const SOCKET_PATH: &str = "/tmp/sandboxd.sock";
const VSOCK_PORT: u32 = 1024;

fn main() {
    // vsock listener for the host (FS-M4 hot-plug path). Optional: the
    // device may be absent (plain boots), then we just skip it.
    match vsock::listen(VSOCK_PORT) {
        Ok(fd) => {
            thread::spawn(move || loop {
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
            });
            eprintln!("guest-proxy: vsock listening on port {}", VSOCK_PORT);
        }
        Err(e) => {
            eprintln!("guest-proxy: vsock unavailable ({}), unix socket only", e);
        }
    }

    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH).expect("bind sandboxd socket");
    std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(0o600))
        .expect("chmod sandboxd socket");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(|| handle(stream));
            }
            Err(e) => {
                eprintln!("accept: {}", e);
                thread::sleep(Duration::from_millis(100));
            }
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

/// {"command":"mount","tag":"<virtiofs tag>","target":"/newroot"}
/// {"command":"umount","target":"/newroot"}
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

    let work_dir = cmd["work_dir"].as_str().unwrap_or("/tmp");
    let timeout = cmd["timeout_secs"].as_u64().unwrap_or(60).min(3600);

    match sandbox::exec_isolated(&args[0], &args, work_dir, timeout) {
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

//! sandboxd — in-guest sandbox runtime daemon.
//!
//! Creates isolated execution environments using Linux namespaces.
//! Phase 1: namespace isolation (mount, UTS, IPC, network).
//! Phase 2+: OverlayFS, cgroup v2, Landlock, seccomp.

mod sandbox;
mod setup;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

const SOCKET_PATH: &str = "/tmp/sandboxd.sock";

fn main() {
    tracing_subscriber::fmt::init();

    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH).expect("bind sandboxd socket");
    tracing::info!(socket = SOCKET_PATH, "sandboxd listening");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(|| handle_client(stream));
            }
            Err(e) => {
                tracing::error!(error = %e, "Accept error");
            }
        }
    }
}

fn handle_client(mut stream: UnixStream) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    if reader.read_line(&mut line).is_err() {
        return;
    }

    let cmd: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(c) => c,
        Err(e) => {
            respond(&mut stream, "error", &format!("Invalid JSON: {}", e), None);
            return;
        }
    };

    let command = cmd["command"].as_str().unwrap_or("");
    match command {
        "exec" => cmd_exec(&mut stream, &cmd),
        "ping" => respond(&mut stream, "ok", "pong", None),
        _ => respond(
            &mut stream,
            "error",
            &format!("Unknown command: {}", command),
            None,
        ),
    }
}

fn cmd_exec(stream: &mut UnixStream, cmd: &serde_json::Value) {
    let args: Vec<String> = match cmd["args"].as_array() {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => {
            respond(stream, "error", "Missing 'args' array", None);
            return;
        }
    };
    if args.is_empty() {
        respond(stream, "error", "Empty args", None);
        return;
    }

    let work_dir = cmd["work_dir"].as_str().unwrap_or("/tmp");

    // Detect which tools are needed from the command args
    let needs_python = args.iter().any(|a| {
        matches!(
            std::path::Path::new(a).file_name().and_then(|n| n.to_str()),
            Some("python3" | "python" | "pip" | "pip3")
        )
    });
    let needs_node = args.iter().any(|a| {
        matches!(
            std::path::Path::new(a).file_name().and_then(|n| n.to_str()),
            Some("node" | "npm" | "npx")
        )
    });

    // Lazy setup for detected tools
    if needs_python || needs_node {
        match setup::setup_tools(needs_python, needs_node) {
            Ok(env) => {
                let wrapped = vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "source {} 2>/dev/null; exec unshare --mount --uts --ipc -- {}",
                        env.activate_script,
                        args.join(" ")
                    ),
                ];
                match sandbox::exec_isolated("sh", &wrapped, work_dir) {
                    Ok(o) => respond(
                        stream,
                        "ok",
                        "command executed",
                        Some(&serde_json::json!({
                            "stdout": o.stdout, "stderr": o.stderr, "exit_code": o.exit_code
                        })),
                    ),
                    Err(e) => respond(stream, "error", &format!("Sandbox error: {}", e), None),
                }
                return;
            }
            Err(e) => {
                respond(stream, "error", &format!("Setup failed: {}", e), None);
                return;
            }
        }
    }

    // Plain execution with namespace isolation
    let mut ns_args: Vec<String> = vec![
        "unshare".into(),
        "--mount".into(),
        "--uts".into(),
        "--ipc".into(),
        "--".into(),
    ];
    ns_args.extend(args);
    match sandbox::exec_isolated("unshare", &ns_args, work_dir) {
        Ok(o) => respond(
            stream,
            "ok",
            "command executed",
            Some(&serde_json::json!({
                "stdout": o.stdout, "stderr": o.stderr, "exit_code": o.exit_code
            })),
        ),
        Err(e) => respond(stream, "error", &format!("Sandbox error: {}", e), None),
    }
}

fn respond(stream: &mut UnixStream, status: &str, message: &str, data: Option<&serde_json::Value>) {
    let mut resp = serde_json::json!({"status": status, "message": message});
    if let Some(d) = data {
        resp["data"] = d.clone();
    }
    let json = serde_json::to_string(&resp).unwrap_or_default();
    let _ = writeln!(stream, "{}", json);
}

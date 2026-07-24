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

    // Parse resource limits
    let memory_mb = cmd["limits"]["memory_mb"].as_u64();
    let cpu_shares = cmd["limits"]["cpu_shares"].as_u64();

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

    // Build cgroup v2 setup prefix for resource limits
    let cg_setup = build_cgroup_setup(memory_mb, cpu_shares);

    // Lazy setup for detected tools
    if needs_python || needs_node {
        match setup::setup_tools(needs_python, needs_node) {
            Ok(env) => {
                let wrapped = vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "{}source {} 2>/dev/null; exec unshare --mount --uts --ipc -- {}",
                        cg_setup,
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

    // Plain execution with namespace isolation + cgroup limits
    let mut ns_args: Vec<String> = vec![
        "unshare".into(),
        "--mount".into(),
        "--uts".into(),
        "--ipc".into(),
    ];
    // Add cgroup setup wrapper if limits are set
    if memory_mb.is_some() || cpu_shares.is_some() {
        ns_args = vec![
            "sh".into(),
            "-c".into(),
            format!("{}{}", cg_setup, args.join(" ")),
        ];
    } else {
        ns_args.push("--".into());
        ns_args.extend(args);
    }
    match sandbox::exec_isolated(&ns_args[0], &ns_args, work_dir) {
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

/// Build shell commands to set up cgroup v2 resource limits.
/// Returns empty string if no limits are set.
fn build_cgroup_setup(memory_mb: Option<u64>, cpu_shares: Option<u64>) -> String {
    if memory_mb.is_none() && cpu_shares.is_none() {
        return String::new();
    }
    let mut setup = String::from("CG=$$; mkdir -p /sys/fs/cgroup/terra-$CG 2>/dev/null; echo $CG > /sys/fs/cgroup/terra-$CG/cgroup.procs 2>/dev/null; ");
    if let Some(mb) = memory_mb {
        setup.push_str(&format!(
            "echo {}M > /sys/fs/cgroup/terra-$CG/memory.max 2>/dev/null; ",
            mb
        ));
    }
    if let Some(shares) = cpu_shares {
        setup.push_str(&format!(
            "echo {} > /sys/fs/cgroup/terra-$CG/cpu.weight 2>/dev/null; ",
            shares
        ));
    }
    setup
}

fn respond(stream: &mut UnixStream, status: &str, message: &str, data: Option<&serde_json::Value>) {
    let mut resp = serde_json::json!({"status": status, "message": message});
    if let Some(d) = data {
        resp["data"] = d.clone();
    }
    let json = serde_json::to_string(&resp).unwrap_or_default();
    let _ = writeln!(stream, "{}", json);
}

//! Daemon mode: listens on a Unix domain socket, accepts JSON commands,
//! dispatches them to the VmManager, and returns JSON responses.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use crate::commands::{execute, Command};
use crate::manager::VmManager;
use crate::pool::WarmPool;

/// Run the controller in daemon mode, listening on the given socket path.
/// Blocks until the socket file is created and listens for connections.
pub fn run(socket_path: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(socket = %socket_path, "Daemon listening");

    let manager = Arc::new(Mutex::new(VmManager::new()));
    let pools = Arc::new(Mutex::new(WarmPool::new()));

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let mgr = Arc::clone(&manager);
                let pools = Arc::clone(&pools);
                std::thread::spawn(move || {
                    handle_client(stream, &mgr, &pools);
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "Accept error");
            }
        }
    }

    Ok(())
}

fn handle_client(
    stream: UnixStream,
    manager: &Arc<Mutex<VmManager>>,
    pools: &Arc<Mutex<WarmPool>>,
) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "Read error");
            return;
        }
    }

    let cmd: Command = match serde_json::from_str(line.trim()) {
        Ok(c) => c,
        Err(e) => {
            let resp = crate::commands::Response::err(format!("Invalid JSON: {}", e));
            let _ = writeln!(
                &stream,
                "{}",
                serde_json::to_string(&resp).unwrap_or_default()
            );
            return;
        }
    };

    tracing::info!(command = %cmd.command, name = ?cmd.name, "Executing command");

    // Handle pool commands separately (they use WarmPool, not VmManager)
    if cmd.command.starts_with("pool_") {
        let mut pools = pools.lock().unwrap();
        let response = crate::commands::pool_execute(&mut pools, cmd);
        let json = serde_json::to_string(&response).unwrap_or_default();
        let _ = writeln!(&stream, "{}", json);
        return;
    }

    let mut mgr = manager.lock().unwrap();
    mgr.reap_dead();
    let response = execute(&mut mgr, cmd);

    let json = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"status":"error","error":"serialization failed"}"#.to_string());
    let _ = writeln!(&stream, "{}", json);
}

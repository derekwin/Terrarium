//! Daemon mode: listens on a Unix domain socket, accepts JSON commands,
//! dispatches them to the VmManager, and returns JSON responses.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use crate::commands::{execute, Command};
use crate::manager::VmManager;

/// Run the controller in daemon mode, listening on the given socket path.
/// Blocks until the socket file is created and listens for connections.
pub fn run(socket_path: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %socket_path, "Daemon listening");

    let manager = Arc::new(Mutex::new(VmManager::new()));

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let mgr = Arc::clone(&manager);
                std::thread::spawn(move || {
                    handle_client(stream, &mgr);
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

    let mut mgr = manager.lock().unwrap();
    mgr.reap_dead();
    let response = execute(&mut mgr, cmd);

    let json = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"status":"error","error":"serialization failed"}"#.to_string());
    let _ = writeln!(&stream, "{}", json);
}

//! Daemon mode: listens on a Unix domain socket, accepts JSON commands,
//! dispatches them to the VmManager, and returns JSON responses.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use adapter_cloud_hypervisor::ChAdapter;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::commands::{execute, Command};
use crate::manager::VmManager;

/// Default CH binary path. Can be overridden via TERRA_CH_BINARY env var.
const DEFAULT_CH_BINARY: &str = "cloud-hypervisor";

/// Run the controller in daemon mode, listening on the given socket path.
pub async fn run(socket_path: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %socket_path, "Daemon listening");

    let ch_binary = std::env::var("TERRA_CH_BINARY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CH_BINARY.to_string());
    let adapter: Arc<dyn adapter_traits::VmAdapter> = Arc::new(ChAdapter::new(ch_binary));
    let manager = Arc::new(Mutex::new(VmManager::new(adapter)));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let mgr = Arc::clone(&manager);
                tokio::spawn(async move {
                    handle_client(stream, &mgr).await;
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "Accept error");
            }
        }
    }
}

async fn handle_client(stream: UnixStream, manager: &Arc<Mutex<VmManager>>) {
    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);
    let mut line = String::new();

    match reader.read_line(&mut line).await {
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
            let json = serde_json::to_string(&resp).unwrap_or_default();
            let _ = writer_half
                .write_all(format!("{}\n", json).as_bytes())
                .await;
            return;
        }
    };

    tracing::info!(command = %cmd.command, name = ?cmd.name, "Executing command");

    let mut mgr = manager.lock().await;
    mgr.reap_dead();
    let response = execute(&mut mgr, cmd).await;

    let json = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"status":"error","error":"serialization failed"}"#.to_string());
    let _ = writer_half
        .write_all(format!("{}\n", json).as_bytes())
        .await;
}

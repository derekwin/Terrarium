//! Daemon mode: listens on a Unix domain socket (local clients) and
//! optionally a TCP address (remote clients), accepts JSON commands,
//! dispatches them to the VmManager, and returns JSON responses.
//!
//! TCP access is gated by a shared token (TERRA_TOKEN): when set, a
//! remote client's first line must be exactly the token, otherwise the
//! connection is closed. The protocol is plaintext — use it only on
//! trusted networks, or SSH-tunnel the unix socket instead.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use adapter_cloud_hypervisor::ChAdapter;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::commands::{execute, Command};
use crate::manager::VmManager;
use terrarium_protocol::Response;

/// Maximum size of a single JSON command line (64 KB).
const MAX_COMMAND_LINE: usize = 64 * 1024;

/// Default CH binary path. Can be overridden via TERRA_CH_BINARY env var.
const DEFAULT_CH_BINARY: &str = "cloud-hypervisor";

/// Run the controller in daemon mode.
///
/// - `socket_path`: unix socket for local clients (chmod 0600)
/// - `tcp_addr`: optional "host:port" for remote clients (token-gated)
pub async fn run(socket_path: &str, tcp_addr: Option<&str>) -> std::io::Result<()> {
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
    let token: Option<String> = std::env::var("TERRA_TOKEN").ok().filter(|s| !s.is_empty());

    // Handle SIGTERM/SIGINT for graceful shutdown.
    let mgr_clone = Arc::clone(&manager);
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        tracing::info!("Received shutdown signal, stopping all VMs");
        mgr_clone.lock().await.shutdown_all().await;
        std::process::exit(0);
    });

    // Optional TCP listener for remote clients.
    if let Some(addr) = tcp_addr {
        let tcp = TcpListener::bind(addr).await?;
        tracing::info!(addr = %addr, token = token.is_some(), "TCP listener for remote clients");
        let mgr = Arc::clone(&manager);
        let token = token.clone();
        tokio::spawn(async move {
            loop {
                match tcp.accept().await {
                    Ok((stream, peer)) => {
                        let mgr = Arc::clone(&mgr);
                        let token = token.clone();
                        tokio::spawn(async move {
                            handle_tcp_client(stream, &mgr, token.as_deref(), &peer.to_string())
                                .await;
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "TCP accept error");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
    } else if token.is_some() {
        tracing::warn!("TERRA_TOKEN is set but no --tcp listener — token has no effect");
    }

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
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Token gate for remote connections: the first line must equal the
/// configured token (when one is set).
async fn handle_tcp_client(
    stream: TcpStream,
    manager: &Arc<Mutex<VmManager>>,
    token: Option<&str>,
    peer: &str,
) {
    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);
    let mut first = String::new();

    match reader.read_line(&mut first).await {
        Ok(0) => return,
        Ok(_) => {
            if first.len() > MAX_COMMAND_LINE {
                let _ = writer_half
                    .write_all(b"{\"status\":\"error\",\"error\":\"request too large\"}\n")
                    .await;
                return;
            }
        }
        Err(_) => return,
    }

    if let Some(expected) = token {
        if first.trim() != expected {
            tracing::warn!(%peer, "Rejected remote client: bad token");
            let _ = writer_half
                .write_all(b"{\"status\":\"error\",\"error\":\"unauthorized\"}\n")
                .await;
            return;
        }
        // Token consumed — the next line is the actual command.
        first.clear();
        match reader.read_line(&mut first).await {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let cmd: Command = match serde_json::from_str(first.trim()) {
        Ok(c) => c,
        Err(e) => {
            let resp = Response::err(format!("Invalid JSON: {}", e));
            let json = serde_json::to_string(&resp).unwrap_or_default();
            let _ = writer_half
                .write_all(format!("{}\n", json).as_bytes())
                .await;
            return;
        }
    };

    tracing::info!(command = %cmd.command, name = ?cmd.name, %peer, "Executing remote command");

    let mut mgr = manager.lock().await;
    mgr.reap_dead();
    let response = execute(&mut mgr, cmd).await;

    let json = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"status":"error","error":"serialization failed"}"#.to_string());
    let _ = writer_half
        .write_all(format!("{}\n", json).as_bytes())
        .await;
}

async fn handle_client(stream: UnixStream, manager: &Arc<Mutex<VmManager>>) {
    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);
    let mut line = String::new();

    match reader.read_line(&mut line).await {
        Ok(0) => return,
        Ok(_) => {
            if line.len() > MAX_COMMAND_LINE {
                let _ = writer_half
                    .write_all(b"{\"status\":\"error\",\"error\":\"request too large\"}\n")
                    .await;
                return;
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Read error");
            return;
        }
    }

    let cmd: Command = match serde_json::from_str(line.trim()) {
        Ok(c) => c,
        Err(e) => {
            let resp = Response::err(format!("Invalid JSON: {}", e));
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

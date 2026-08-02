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

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::commands::{
    execute, prepare_blocking_exec, prepared_exec_audited, Command, PreparedBlockingExec,
};
use crate::manager::VmManager;
use terrarium_protocol::Response;

/// Maximum size of a single JSON command line (64 KB).
const MAX_COMMAND_LINE: usize = 64 * 1024;

/// Commands that can create or remove VMs, or otherwise change VM
/// liveness. Only these trigger the O(n) `reap_dead` liveness scan —
/// read-only commands (`list`, `info`, `exec`, `session_*`, ...) skip it.
const REAP_COMMANDS: &[&str] = &[
    "create",
    "kill",
    "shutdown",
    "destroy",
    "pool_create",
    "pool_release",
    "sandbox_create",
    "tenant_destroy",
];

/// Run the controller in daemon mode.
///
/// - `socket_path`: unix socket for local clients (chmod 0600)
/// - `tcp_addr`: optional "host:port" for remote clients (token-gated)
/// - `adapter`: VMM adapter (testable with MockVmAdapter)
/// - `embedded`: true when the daemon runs inside a host process (PyO3
///   FFI). In embedded mode `daemon_stop` is refused, because tearing
///   down the process would kill the host.
///
/// Shutdown (SIGTERM/SIGINT or a `daemon_stop` command) converges on a
/// single flow: the shutdown watch channel flips, both accept loops
/// exit promptly, `shutdown_all` runs, and `run` returns.
pub async fn run(
    socket_path: &str,
    tcp_addr: Option<&str>,
    adapter: Arc<dyn adapter_traits::VmAdapter>,
    embedded: bool,
) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %socket_path, "Daemon listening");

    let manager = Arc::new(Mutex::new(VmManager::new(adapter, "/tmp".to_string())));
    let token: Option<String> = std::env::var("TERRA_TOKEN").ok().filter(|s| !s.is_empty());

    // Shutdown signal: SIGTERM/SIGINT flip the watch channel; the main
    // loop below notices (even while blocked in accept) and performs the
    // actual cleanup, so signal and `daemon_stop` share one flow.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    {
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
            tracing::info!("Received shutdown signal");
            let _ = shutdown_tx.send(true);
        });
    }

    // Optional TCP listener for remote clients.
    if let Some(addr) = tcp_addr {
        let tcp = TcpListener::bind(addr).await?;
        tracing::info!(addr = %addr, token = token.is_some(), "TCP listener for remote clients");
        let mgr = Arc::clone(&manager);
        let token = token.clone();
        let mut shutdown_rx_tcp = shutdown_rx.clone();
        let shutdown_tx_tcp = shutdown_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx_tcp.changed() => break,
                    accepted = tcp.accept() => match accepted {
                        Ok((stream, peer)) => {
                            let mgr = Arc::clone(&mgr);
                            let token = token.clone();
                            let shutdown_tx = shutdown_tx_tcp.clone();
                            tokio::spawn(async move {
                                handle_tcp_client(
                                    stream,
                                    &mgr,
                                    token.as_deref(),
                                    &peer.to_string(),
                                    embedded,
                                    &shutdown_tx,
                                )
                                .await;
                            });
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "TCP accept error");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    },
                }
            }
        });
    } else if token.is_some() {
        tracing::warn!("TERRA_TOKEN is set but no --tcp listener — token has no effect");
    }

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    let mgr = Arc::clone(&manager);
                    let shutdown_tx = shutdown_tx.clone();
                    tokio::spawn(async move {
                        handle_client(stream, &mgr, embedded, &shutdown_tx).await;
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "Accept error");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }

    tracing::info!("Shutting down, stopping all VMs");
    manager.lock().await.shutdown_all().await;
    Ok(())
}

/// Dispatch one parsed command. `daemon_stop` is intercepted here because
/// it needs the daemon's embedded flag; everything else goes through the
/// shared `execute`. Returns the response plus whether a daemon stop was
/// requested (the caller triggers the shutdown channel after writing the
/// response, so the client still receives it).
async fn dispatch(
    cmd: Command,
    manager: &Arc<Mutex<VmManager>>,
    embedded: bool,
) -> (Response, bool) {
    if cmd.command == "daemon_stop" {
        if embedded {
            return (
                Response::err("daemon_stop is not supported in embedded mode (in-process daemon)"),
                false,
            );
        }
        return (Response::ok_msg("daemon shutting down"), true);
    }
    let mut mgr = manager.lock().await;
    // Best-effort liveness scan, only before commands that can change VM
    // state — read-only commands skip the O(n) `Arc::get_mut` scan.
    if REAP_COMMANDS.contains(&cmd.command.as_str()) {
        mgr.reap_dead();
    }

    // Blocking exec is the one command that can run for up to its full
    // timeout (3600s). Resolve everything under the lock — handle Arc +
    // ExecOpts — then drop the lock before awaiting `handle.exec`, so a
    // long-running exec no longer serializes every other command behind
    // `Mutex<VmManager>`. Background execs keep the shared `execute` path
    // below: they register their session under the lock and return
    // immediately (their spawned task already runs lock-free), and an
    // invalid exec_mode must still produce the shared-path error.
    if matches!(cmd.command.as_str(), "exec" | "sandbox_exec")
        && cmd.exec_mode.as_deref().unwrap_or("blocking") == "blocking"
    {
        match prepare_blocking_exec(&mgr, &cmd) {
            Ok(prepared) => {
                let PreparedBlockingExec {
                    prepared,
                    policy,
                    audit_id,
                } = prepared;
                drop(mgr); // the exec itself runs without the manager lock
                return (
                    prepared_exec_audited(prepared, policy.as_ref(), &audit_id, &cmd.args).await,
                    false,
                );
            }
            Err(resp) => return (resp, false),
        }
    }

    (execute(&mut mgr, cmd).await, false)
}

/// Token gate for remote connections: the first line must equal the
/// configured token (when one is set).
async fn handle_tcp_client(
    stream: TcpStream,
    manager: &Arc<Mutex<VmManager>>,
    token: Option<&str>,
    peer: &str,
    embedded: bool,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
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

    let (response, stop) = dispatch(cmd, manager, embedded).await;

    let json = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"status":"error","error":"serialization failed"}"#.to_string());
    let _ = writer_half
        .write_all(format!("{}\n", json).as_bytes())
        .await;
    if stop {
        let _ = shutdown_tx.send(true);
    }
}

async fn handle_client(
    stream: UnixStream,
    manager: &Arc<Mutex<VmManager>>,
    embedded: bool,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
) {
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

    let (response, stop) = dispatch(cmd, manager, embedded).await;

    let json = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"status":"error","error":"serialization failed"}"#.to_string());
    let _ = writer_half
        .write_all(format!("{}\n", json).as_bytes())
        .await;
    if stop {
        let _ = shutdown_tx.send(true);
    }
}

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
use tokio::sync::{Mutex, MutexGuard};

use crate::commands::sandbox::{
    bind_sandbox, finalize_tenant_destroy, finish_sandbox_create, finish_tenant_destroy,
    prepare_sandbox_create, prepare_tenant_destroy, TenantVm,
};
use crate::commands::{
    batch::{
        batch_create_response, batch_recycle_rows, bind_batch_envs, prepare_batch_create,
        prepare_batch_exec, prepare_batch_recycle, run_batch_execs, run_batch_recycle,
        spawn_batch_vms, BatchRecycleItem,
    },
    execute,
    pool::{
        finish_pool_create_snapshot, pool_create_snapshot_response, prepare_pool_create_snapshot,
    },
    prepare_blocking_exec, prepare_lifecycle, prepared_exec_audited, require_name, Command,
    PreparedBlockingExec, PreparedLifecycle,
};
use crate::manager::VmManager;
use adapter_traits::{VmHandle, VmSpec};
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
    "pool_create_snapshot",
    "pool_release",
    "sandbox_create",
    "tenant_destroy",
    "batch_create",
    "batch_recycle",
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

    // Must match the adapter's TERRA_SNAPSHOT_DIR: the snapshot dir is
    // both the engine-side default destination AND the CH Landlock
    // whitelist root. A mismatch silently sends CH to a path outside its
    // whitelist (Landlock EPERM on every snapshot).
    let snapshot_dir = std::env::var("TERRA_SNAPSHOT_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    let manager = Arc::new(Mutex::new(VmManager::new(adapter, snapshot_dir)));
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

/// Owns `Mutex<VmManager>`: the ONLY way dispatch code can lock/unlock.
/// Handler functions receive only ``&mut LockGuard`` — never the raw
/// ``&Mutex`` — so a stray ``manager.lock().await`` cannot even compile
/// inside a handler, and a double-lock PANICS loudly instead of silently
/// deadlocking (tokio Mutex is not reentrant).
struct LockGuard<'a> {
    manager: &'a Mutex<VmManager>,
    guard: Option<MutexGuard<'a, VmManager>>,
}

impl<'a> LockGuard<'a> {
    fn new(manager: &'a Mutex<VmManager>) -> Self {
        Self {
            manager,
            guard: None,
        }
    }

    /// Acquire the manager lock. Panics on a double-lock (the guard must
    /// be `take()`n/dropped first).
    async fn lock(&mut self) {
        assert!(
            self.guard.is_none(),
            "LockGuard double-lock: drop the taken guard first"
        );
        self.guard = Some(self.manager.lock().await);
    }

    /// Take the guard out for under-lock work. The mutex is released when
    /// the returned guard is dropped.
    fn take(&mut self) -> MutexGuard<'a, VmManager> {
        self.guard.take().expect("LockGuard not currently locked")
    }

    /// Release + re-acquire around slow outside work. Panics if the guard
    /// was not released first (i.e. a missing `drop`).
    async fn relock(&mut self) {
        assert!(
            self.guard.is_none(),
            "LockGuard relock while still locked: drop the guard first"
        );
        self.guard = Some(self.manager.lock().await);
    }
}

/// Dispatch one parsed command. `daemon_stop` is intercepted here because
/// it needs the daemon's embedded flag; everything else dispatches to a
/// per-command handler. Lock-free handlers receive ``&mut LockGuard`` (not
/// the raw mutex), so re-entrant locking is structurally impossible.
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

    let mut lock = LockGuard::new(manager);
    lock.lock().await;

    // Best-effort liveness scan, only before commands that can change VM
    // state — read-only commands skip the O(n) scan.
    if REAP_COMMANDS.contains(&cmd.command.as_str()) {
        let mut mgr = lock.take();
        mgr.reap_dead();
        drop(mgr);
        // The reap released the guard; re-acquire so the handler below can
        // take() it for its under-lock phase.
        lock.lock().await;
    }

    match cmd.command.as_str() {
        "create" | "restore" => handle_create_restore(&mut lock, &cmd).await,
        "sandbox_create" => handle_sandbox_create(&mut lock, &cmd).await,
        "destroy" | "shutdown" | "kill" => handle_teardown(&mut lock, &cmd).await,
        "reset_vm" => handle_reset_vm(&mut lock, &cmd).await,
        "tenant_destroy" => handle_tenant_destroy(&mut lock, &cmd).await,
        "pool_create_snapshot" => handle_pool_create_snapshot(&mut lock, &cmd).await,
        "batch_create" => handle_batch_create(&mut lock, &cmd).await,
        "batch_exec" => handle_batch_exec(&mut lock, &cmd).await,
        "batch_recycle" => handle_batch_recycle(&mut lock, &cmd).await,
        "exec" | "sandbox_exec" if cmd.exec_mode.as_deref().unwrap_or("blocking") == "blocking" => {
            handle_blocking_exec(&mut lock, &cmd).await
        }
        _ => {
            // Default: the shared execute() path holds the lock for the
            // whole command (read-only or non-hot-path commands).
            let mut mgr = lock.take();
            (execute(&mut mgr, cmd).await, false)
        }
    }
}

async fn handle_create_restore(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mgr = lock.take();
    match prepare_lifecycle(&mgr, cmd) {
        Ok(prepared) => {
            let adapter = mgr.adapter();
            drop(mgr);
            let handle = match &prepared {
                PreparedLifecycle::Create(spec) => adapter.create(spec).await,
                PreparedLifecycle::Restore { snapshot, spec } => {
                    adapter.restore(snapshot, spec).await
                }
            };
            let handle = match handle {
                Ok(h) => h,
                Err(e) => return (Response::err(e.to_string()), false),
            };
            let spec = match &prepared {
                PreparedLifecycle::Create(spec) => spec,
                PreparedLifecycle::Restore { spec, .. } => spec,
            };
            lock.relock().await;
            let mut mgr = lock.take();
            if let Err(e) = mgr.register_vm(spec, handle) {
                return (Response::err(e.to_string()), false);
            }
            let name = spec.name.to_string();
            let pid = mgr.get(&name).map(|h| h.pid());
            let resp = match prepared {
                PreparedLifecycle::Create(_) => Response::ok(serde_json::json!({
                    "name": name,
                    "status": "created",
                    "pid": pid,
                })),
                PreparedLifecycle::Restore { .. } => Response::ok(serde_json::json!({
                    "name": name,
                    "status": "restored",
                    "pid": pid,
                })),
            };
            (resp, false)
        }
        Err(resp) => (resp, false),
    }
}

async fn handle_sandbox_create(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mut mgr = lock.take();
    match prepare_sandbox_create(&mut mgr, cmd).await {
        Ok(prepared) => {
            let vm_adapter = mgr.adapter();
            let sb_adapter = mgr.sandbox_adapter_arc();
            drop(mgr);

            // Cold boot outside the lock (concurrent creates run in
            // parallel; the existing-VM/pool path has no spawn).
            let mut spawned: Option<(VmSpec, Box<dyn VmHandle>)> = None;
            if let TenantVm::ColdSpec { spec, .. } = &prepared.vm {
                match vm_adapter.create(spec).await {
                    Ok(h) => spawned = Some((spec.clone(), h)),
                    Err(e) => return (Response::err(e.to_string()), false),
                }
            }

            // Brief re-lock: register a freshly booted VM + resolve the
            // Arc handle for the session binding below.
            lock.relock().await;
            let mut mgr = lock.take();
            let handle = match &prepared.vm {
                TenantVm::Existing { handle, .. } => handle.clone(),
                TenantVm::ColdSpec { vm_name, spec } => {
                    let (_, h) = spawned
                        .take()
                        .expect("a ColdSpec must have been spawned above");
                    if let Err(e) = mgr.register_vm(spec, h) {
                        return (Response::err(e.to_string()), false);
                    }
                    match mgr.get_handle(vm_name) {
                        Some(h) => h,
                        None => {
                            return (Response::err(format!("VM '{}' not found", vm_name)), false)
                        }
                    }
                }
            };
            drop(mgr);

            // Slow, lock-free: L2 session + workdir (agent-boot retries
            // live in bind_sandbox).
            let record = match bind_sandbox(&*sb_adapter, handle, &prepared).await {
                Ok(r) => r,
                Err(resp) => return (resp, false),
            };

            // Re-lock only to register the sandbox record + audit.
            lock.relock().await;
            let mut mgr = lock.take();
            let resp = finish_sandbox_create(&mut mgr, &prepared, record);
            (resp, false)
        }
        Err(resp) => (resp, false),
    }
}

async fn handle_teardown(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mut mgr = lock.take();
    let name = match require_name(cmd) {
        Ok(n) => n,
        Err(resp) => return (resp, false),
    };
    let handle = match mgr.unregister(&name) {
        Some(h) => h,
        None => {
            let err = adapter_traits::AdapterError::not_found(format!("VM '{}' not found", name))
                .to_string();
            return (Response::err(err), false);
        }
    };
    let snap_dir = format!("{}/terra-snap-{}", mgr.snapshot_dir(), name);
    drop(mgr);
    if cmd.command == "destroy" {
        // Snapshot artifacts of this VM are garbage (best-effort).
        let _ = std::fs::remove_dir_all(&snap_dir);
    }
    let msg = match cmd.command.as_str() {
        "destroy" | "shutdown" => match handle.shutdown().await {
            Ok(()) => format!(
                "VM '{}' {}",
                name,
                if cmd.command == "destroy" {
                    "destroyed"
                } else {
                    "shut down"
                }
            ),
            Err(e) => return (Response::err(e.to_string()), false),
        },
        "kill" => {
            drop(handle); // the backend's Drop kills the process
            format!("VM '{}' killed", name)
        }
        _ => unreachable!(),
    };
    (Response::ok_msg(&msg), false)
}

async fn handle_reset_vm(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mgr = lock.take();
    let name = match require_name(cmd) {
        Ok(n) => n,
        Err(resp) => return (resp, false),
    };
    let handle = match mgr.get_handle(&name) {
        Some(h) => h,
        None => {
            let err = adapter_traits::AdapterError::not_found(format!("VM '{}' not found", name))
                .to_string();
            return (Response::err(err), false);
        }
    };
    drop(mgr);
    match handle.reset_fs().await {
        Ok(()) => (
            Response::ok(serde_json::json!({"name": name, "status": "reset"})),
            false,
        ),
        Err(e) => (Response::err(e.to_string()), false),
    }
}

async fn handle_tenant_destroy(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mut mgr = lock.take();
    match prepare_tenant_destroy(&mut mgr, cmd) {
        Ok(plan) => {
            drop(mgr);
            let released = match finish_tenant_destroy(&plan).await {
                Ok(r) => r,
                Err(e) => return (Response::err(e), false),
            };
            lock.relock().await;
            let mut mgr = lock.take();
            finalize_tenant_destroy(&mut mgr, &plan);
            let resp = Response::ok(serde_json::json!({
                "tenant": plan.tenant,
                "vm": plan.vm_name,
                "sandboxes_removed": plan.removed,
                "released_to_pool": released,
                "status": if released { "released" } else { "destroyed" },
            }));
            (resp, false)
        }
        Err(resp) => (resp, false),
    }
}

async fn handle_pool_create_snapshot(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mut mgr = lock.take();
    match prepare_pool_create_snapshot(&mut mgr, cmd) {
        Ok(plan) => {
            let adapter = mgr.adapter();
            drop(mgr);
            let results = finish_pool_create_snapshot(&plan, adapter).await;
            lock.relock().await;
            let mut mgr = lock.take();
            let mut ready = Vec::new();
            let mut failed = Vec::new();
            for (spec, result) in results {
                match result {
                    Some((handle, Ok(()))) => {
                        match mgr.pool_register_ready(&spec, handle, plan.layers.clone(), plan.net)
                        {
                            Ok(()) => ready.push(spec.name.to_string()),
                            Err(e) => failed.push((spec.name.to_string(), e.to_string())),
                        }
                    }
                    Some((handle, Err(e))) => {
                        drop(handle);
                        failed.push((spec.name.to_string(), e));
                    }
                    None => failed.push((spec.name.to_string(), "restore failed".to_string())),
                }
            }
            let resp = pool_create_snapshot_response(ready, failed);
            (resp, false)
        }
        Err(resp) => (resp, false),
    }
}

async fn handle_batch_create(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mut mgr = lock.take();
    match prepare_batch_create(&mut mgr, cmd).await {
        Ok(envs) => {
            let adapter = mgr.adapter();
            let sb_adapter = mgr.sandbox_adapter_arc();
            drop(mgr);
            let spawned = spawn_batch_vms(&envs, adapter).await;
            lock.relock().await;
            let mut mgr = lock.take();
            let mut handles = std::collections::HashMap::new();
            let mut failed: Vec<(String, String)> = Vec::new();
            for (idx, handle) in spawned {
                if let TenantVm::ColdSpec { spec, .. } = &envs[idx].plan.vm {
                    match mgr.register_vm(spec, handle) {
                        Ok(()) => {
                            let vm = match &envs[idx].plan.vm {
                                TenantVm::ColdSpec { vm_name, .. } => vm_name.clone(),
                                _ => String::new(),
                            };
                            if let Some(h) = mgr.get_handle(&vm) {
                                handles.insert(idx, h);
                            }
                        }
                        Err(e) => failed.push((envs[idx].tenant.clone(), e.to_string())),
                    }
                }
            }
            for (idx, env) in envs.iter().enumerate() {
                if let TenantVm::Existing { handle, .. } = &env.plan.vm {
                    handles.insert(idx, handle.clone());
                }
            }
            drop(mgr);
            let results = bind_batch_envs(&envs, &handles, sb_adapter).await;
            lock.relock().await;
            let mut mgr = lock.take();
            let mut created = Vec::new();
            for (idx, r) in results {
                match r {
                    Ok(record) => {
                        crate::commands::sandbox::finish_sandbox_create(
                            &mut mgr,
                            &envs[idx].plan,
                            record,
                        );
                        let vm = match &envs[idx].plan.vm {
                            TenantVm::Existing { vm_name, .. } => vm_name.clone(),
                            TenantVm::ColdSpec { vm_name, .. } => vm_name.clone(),
                        };
                        created.push(serde_json::json!({
                            "tenant": envs[idx].tenant,
                            "id": envs[idx].plan.sandbox_id,
                            "vm": vm,
                        }));
                    }
                    Err(e) => failed.push((envs[idx].tenant.clone(), e)),
                }
            }
            let resp = batch_create_response(created, failed);
            (resp, false)
        }
        Err(resp) => (resp, false),
    }
}

async fn handle_batch_exec(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mgr = lock.take();
    match prepare_batch_exec(&mgr, cmd) {
        Ok(envs) => {
            drop(mgr);
            let results = run_batch_execs(envs).await;
            let rows: Vec<_> = results
                .into_iter()
                .map(|(id, resp)| {
                    serde_json::json!({"id": id, "status": resp.status, "data": resp.data})
                })
                .collect();
            (
                Response::ok(serde_json::json!({"results": rows, "count": rows.len()})),
                false,
            )
        }
        Err(resp) => (resp, false),
    }
}

async fn handle_batch_recycle(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mut mgr = lock.take();
    match prepare_batch_recycle(&mut mgr, cmd) {
        Ok(items) => {
            drop(mgr);
            let results = run_batch_recycle(&items).await;
            lock.relock().await;
            let mut mgr = lock.take();
            for item in &items {
                if let BatchRecycleItem::Destroy(plan) = item {
                    crate::commands::sandbox::finalize_tenant_destroy(&mut mgr, plan);
                }
            }
            let mode = cmd.mode.clone().unwrap_or_else(|| "destroy".to_string());
            let rows = batch_recycle_rows(results, &mode);
            (
                Response::ok(serde_json::json!({"results": rows, "count": rows.len()})),
                false,
            )
        }
        Err(resp) => (resp, false),
    }
}

async fn handle_blocking_exec(lock: &mut LockGuard<'_>, cmd: &Command) -> (Response, bool) {
    let mgr = lock.take();
    match prepare_blocking_exec(&mgr, cmd) {
        Ok(prepared) => {
            let PreparedBlockingExec {
                prepared,
                policy,
                audit_id,
            } = prepared;
            drop(mgr); // the exec itself runs without the manager lock
            (
                prepared_exec_audited(prepared, policy.as_ref(), &audit_id, &cmd.args).await,
                false,
            )
        }
        Err(resp) => (resp, false),
    }
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

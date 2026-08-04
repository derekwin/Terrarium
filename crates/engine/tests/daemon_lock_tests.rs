//! Concurrency regression test: a long-running blocking exec must NOT hold
//! the daemon's global `Mutex<VmManager>` while it runs.
//!
//! The daemon dispatch used to `execute()` every command while holding the
//! lock, so a blocking exec (up to its full 3600s timeout) serialized all
//! other commands behind it. The fix resolves the exec's handle + options
//! under the lock and then runs `handle.exec` lock-free (like background
//! execs already did). This test parks a blocking exec on a gate, then
//! proves a second command is served while the exec is still in flight —
//! with the old code the second command hangs behind the lock and the
//! timeout below fires.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::MockVmAdapter;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Wait for the daemon socket to appear.
async fn wait_for_socket(socket: &str) {
    for _ in 0..100 {
        if Path::new(socket).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon socket {} did not appear", socket);
}

/// Send one JSON command line and read the JSON response line.
async fn roundtrip(socket: &str, command: &str) -> serde_json::Value {
    let stream = UnixStream::connect(socket)
        .await
        .expect("connect to daemon socket");
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(format!("{}\n", command).as_bytes())
        .await
        .expect("write command");
    let mut line = String::new();
    BufReader::new(reader)
        .read_line(&mut line)
        .await
        .expect("read response");
    serde_json::from_str(line.trim()).expect("response is valid JSON")
}

/// A blocking exec parked on a gate (still in flight) must not block a
/// second command: `list` is served while the exec is unresolved.
#[tokio::test]
async fn test_second_command_served_during_blocking_exec() {
    let socket = format!("/tmp/terra-test-lock-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);

    let gate = Arc::new(tokio::sync::Notify::new());
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("done\n", "", 0)
        .with_blocking_exec_gate(gate.clone());
    let exec_log = adapter.exec_log();

    let sock = socket.clone();
    let daemon = tokio::spawn(async move {
        terrarium_engine::daemon::run(&sock, None, Arc::new(adapter), false).await
    });
    wait_for_socket(&socket).await;

    let resp = roundtrip(
        &socket,
        r#"{"command":"create","name":"lock-vm","kernel":"/fake/vmlinux"}"#,
    )
    .await;
    assert_eq!(resp["status"], "ok", "create should succeed: {resp}");

    // Blocking exec (default mode): parks on the gate, so it stays
    // in flight until we release it.
    let exec_socket = socket.clone();
    let exec_task = tokio::spawn(async move {
        roundtrip(
            &exec_socket,
            r#"{"command":"exec","name":"lock-vm","args":["sleep","3600"],"timeout_secs":3600}"#,
        )
        .await
    });

    // Wait until the exec has actually reached the mock guest (it pushes
    // the log entry before parking) — i.e. it is genuinely in flight.
    for _ in 0..100 {
        if !exec_log.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        exec_log.lock().unwrap().len(),
        1,
        "blocking exec should be in flight"
    );

    // While the blocking exec is parked, a second command must be served
    // immediately. Old code: dispatch held the lock across the exec, so
    // this times out. Fixed code: the exec runs lock-free, so it returns.
    let resp = tokio::time::timeout(
        Duration::from_secs(3),
        roundtrip(&socket, r#"{"command":"list"}"#),
    )
    .await
    .expect("second command must be served while a blocking exec is in flight");
    assert_eq!(resp["status"], "ok", "list during in-flight exec: {resp}");

    // Release the gate: the blocking exec completes with its configured
    // result and the client receives it.
    gate.notify_one();
    let exec_resp = tokio::time::timeout(Duration::from_secs(5), exec_task)
        .await
        .expect("blocking exec should complete after gate release")
        .expect("exec task panicked");
    assert_eq!(exec_resp["status"], "ok", "exec response: {exec_resp}");
    assert_eq!(exec_resp["data"]["stdout"], "done\n");

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

/// A VM lifecycle call (create) parked on the mock gate must not block a
/// second command: the daemon resolves the spec under the lock, calls the
/// adapter outside it, and registers afterwards (P1-2 batch scale).
#[tokio::test]
async fn test_second_command_served_during_create() {
    let socket = format!("/tmp/terra-test-create-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);

    let gate = Arc::new(tokio::sync::Notify::new());
    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_create_gate(gate.clone());

    let sock = socket.clone();
    let daemon = tokio::spawn(async move {
        terrarium_engine::daemon::run(&sock, None, Arc::new(adapter), false).await
    });
    wait_for_socket(&socket).await;

    // create parks on the gate, so it stays in flight.
    let create_socket = socket.clone();
    let create_task = tokio::spawn(async move {
        roundtrip(
            &create_socket,
            r#"{"command":"create","name":"lc-vm","kernel":"/fake/vmlinux"}"#,
        )
        .await
    });

    // Let the create reach the adapter (gate park).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // While the create is parked, a second command must be served
    // immediately. Old code held the lock across the adapter call.
    let resp = tokio::time::timeout(
        Duration::from_secs(3),
        roundtrip(&socket, r#"{"command":"list"}"#),
    )
    .await
    .expect("second command must be served while a create is in flight");
    assert_eq!(resp["status"], "ok", "list during in-flight create: {resp}");

    // Release: create completes and registers the VM.
    gate.notify_one();
    let create_resp = tokio::time::timeout(Duration::from_secs(5), create_task)
        .await
        .expect("create should complete after gate release")
        .expect("create task panicked");
    assert_eq!(
        create_resp["status"], "ok",
        "create response: {create_resp}"
    );
    assert_eq!(create_resp["data"]["status"], "created");

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

/// Two concurrent creates with the same name: exactly one wins; the loser
/// gets the duplicate-name error and its orphan handle is dropped.
#[tokio::test]
async fn test_concurrent_same_name_create_one_wins() {
    let socket = format!("/tmp/terra-test-dup-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);

    let adapter = MockVmAdapter::new().with_state("Running");
    let sock = socket.clone();
    let daemon = tokio::spawn(async move {
        terrarium_engine::daemon::run(&sock, None, Arc::new(adapter), false).await
    });
    wait_for_socket(&socket).await;

    let cmd = r#"{"command":"create","name":"dup-vm","kernel":"/fake/vmlinux"}"#;
    let s1 = socket.clone();
    let s2 = socket.clone();
    let a = tokio::spawn(async move { roundtrip(&s1, cmd).await });
    let b = tokio::spawn(async move { roundtrip(&s2, cmd).await });
    let (ra, rb) = tokio::join!(a, b);
    let ra = ra.expect("create task A panicked");
    let rb = rb.expect("create task B panicked");
    let ok_count = [&ra, &rb]
        .into_iter()
        .filter(|r| r["status"] == "ok")
        .count();
    assert_eq!(ok_count, 1, "exactly one concurrent same-name create wins");
    let err = [&ra, &rb]
        .into_iter()
        .find(|r| r["status"] != "ok")
        .expect("loser errors");
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("already exists"),
        "loser error: {err}"
    );

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

/// M2 on the daemon's lock-free blocking path: `prepare_blocking_exec`
/// validates the ACTUAL executed policy of a sandboxed blocking
/// `sandbox_exec` against the tenant VM's quota before resolving the
/// handle. A per-call override declaring more memory than the 512 MB
/// default quota must fail with the `validate_with_vm` message, and the
/// exec must never reach the guest.
#[tokio::test]
async fn test_daemon_blocking_sandbox_exec_over_vm_quota_rejected() {
    let socket = format!("/tmp/terra-test-quota-sb-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);

    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("done\n", "", 0);
    let exec_log = adapter.exec_log();

    let sock = socket.clone();
    let daemon = tokio::spawn(async move {
        terrarium_engine::daemon::run(&sock, None, Arc::new(adapter), false).await
    });
    wait_for_socket(&socket).await;

    let resp = roundtrip(
        &socket,
        r#"{"command":"create","name":"quota-vm","kernel":"/fake/vmlinux"}"#,
    )
    .await;
    assert_eq!(resp["status"], "ok", "create should succeed: {resp}");

    let resp = roundtrip(
        &socket,
        r#"{"command":"sandbox_create","tenant":"research","name":"unused","kernel":"/fake/vmlinux"}"#,
    )
    .await;
    assert_eq!(resp["status"], "ok", "sandbox_create: {resp}");
    let id = resp["data"]["id"].as_str().unwrap().to_string();
    exec_log.lock().unwrap().clear(); // drop the workdir mkdir call

    let resp = roundtrip(
        &socket,
        &format!(
            r#"{{"command":"sandbox_exec","id":"{}","args":["echo","hi"],"policy":{{"capabilities":[],"limits":{{"memory_mb":1024}},"default":"deny","audit":{{"deny":false,"exec":false,"resource":false}},"version":1}}}}"#,
            id
        ),
    )
    .await;
    assert_eq!(resp["status"], "error", "over-quota override must fail");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("exceeds VM quota"),
        "validate_with_vm message expected: {resp}"
    );
    assert!(
        exec_log.lock().unwrap().is_empty(),
        "no guest exec may run for an over-quota override"
    );

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

/// M2 on the direct variant of `prepare_blocking_exec`: a sandboxed
/// blocking `exec` (no sandbox id) with an over-quota policy runs the
/// computed effective policy — the quota check must reject it before the
/// guest exec.
#[tokio::test]
async fn test_daemon_blocking_sandboxed_exec_over_vm_quota_rejected() {
    let socket = format!("/tmp/terra-test-quota-exec-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);

    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("done\n", "", 0);
    let exec_log = adapter.exec_log();

    let sock = socket.clone();
    let daemon = tokio::spawn(async move {
        terrarium_engine::daemon::run(&sock, None, Arc::new(adapter), false).await
    });
    wait_for_socket(&socket).await;

    let resp = roundtrip(
        &socket,
        r#"{"command":"create","name":"quota-vm","kernel":"/fake/vmlinux"}"#,
    )
    .await;
    assert_eq!(resp["status"], "ok", "create should succeed: {resp}");
    exec_log.lock().unwrap().clear();

    let resp = roundtrip(
        &socket,
        r#"{"command":"exec","name":"quota-vm","args":["echo","hi"],"sandbox":true,"policy":{"capabilities":[],"limits":{"memory_mb":1024},"default":"deny","audit":{"deny":false,"exec":false,"resource":false},"version":1}}"#,
    )
    .await;
    assert_eq!(resp["status"], "error", "over-quota exec must fail");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("exceeds VM quota"),
        "validate_with_vm message expected: {resp}"
    );
    assert!(
        exec_log.lock().unwrap().is_empty(),
        "no guest exec may run for an over-quota policy"
    );

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}
#[tokio::test]
async fn test_blocking_exec_sandboxed_injects_default_policy() {
    let socket = format!("/tmp/terra-test-inject-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);

    let adapter = MockVmAdapter::new()
        .with_state("Running")
        .with_exec("done\n", "", 0);
    let exec_log = adapter.exec_log();

    let sock = socket.clone();
    let daemon = tokio::spawn(async move {
        terrarium_engine::daemon::run(&sock, None, Arc::new(adapter), false).await
    });
    wait_for_socket(&socket).await;

    let resp = roundtrip(
        &socket,
        r#"{"command":"create","name":"inject-vm","kernel":"/fake/vmlinux"}"#,
    )
    .await;
    assert_eq!(resp["status"], "ok", "create should succeed: {resp}");

    // sandbox:true + no policy → the guest receives the engine default.
    let resp = roundtrip(
        &socket,
        r#"{"command":"exec","name":"inject-vm","args":["echo","hi"],"sandbox":true}"#,
    )
    .await;
    assert_eq!(resp["status"], "ok", "sandboxed exec: {resp}");
    {
        let log = exec_log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].sandbox);
        assert_eq!(
            log[0].policy.as_ref(),
            Some(&terrarium_engine::policy::default_sandbox_policy()),
            "sandboxed exec with no policy must receive the engine default"
        );
    }

    // sandbox:false + no policy → the guest receives no policy.
    let resp = roundtrip(
        &socket,
        r#"{"command":"exec","name":"inject-vm","args":["echo","hi"],"sandbox":false}"#,
    )
    .await;
    assert_eq!(resp["status"], "ok", "unsandboxed exec: {resp}");
    {
        let log = exec_log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(!log[1].sandbox);
        assert_eq!(
            log[1].policy, None,
            "unsandboxed exec must stay policy-free"
        );
    }

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

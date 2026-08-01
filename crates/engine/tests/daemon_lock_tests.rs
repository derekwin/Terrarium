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

/// D2 parity on the daemon's lock-free blocking path: `prepare_blocking_exec`
/// injects the engine default for a sandboxed `exec` with no policy — the
/// injection keys on the sandbox flag, not on the command name. An
/// unsandboxed `exec` reaches the guest policy-free.
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

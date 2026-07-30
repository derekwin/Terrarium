//! Integration tests for `terrarium_engine::daemon::run` shutdown behavior.
//!
//! Uses the shared `MockVmAdapter` so no KVM / Cloud Hypervisor is needed.

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

/// daemon_stop (non-embedded): client gets an ok response, then the daemon
/// exits promptly — the accept loop must not hang waiting for another client.
#[tokio::test]
async fn test_daemon_stop_exits_daemon() {
    let socket = format!("/tmp/terra-test-daemon-stop-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);

    let adapter = Arc::new(MockVmAdapter::new());
    let sock = socket.clone();
    let handle =
        tokio::spawn(
            async move { terrarium_engine::daemon::run(&sock, None, adapter, false).await },
        );

    wait_for_socket(&socket).await;
    let resp = roundtrip(&socket, r#"{"command":"daemon_stop"}"#).await;
    assert_eq!(
        resp["status"], "ok",
        "daemon_stop should be accepted: {resp}"
    );

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("daemon did not exit within 5s after daemon_stop");
    result
        .expect("daemon task panicked")
        .expect("daemon returned an io error");
    let _ = std::fs::remove_file(&socket);
}

/// daemon_stop in embedded mode: refused with an explicit error, and the
/// daemon keeps serving (stopping would kill the host process).
#[tokio::test]
async fn test_daemon_stop_refused_when_embedded() {
    let socket = format!(
        "/tmp/terra-test-daemon-stop-embedded-{}.sock",
        std::process::id()
    );
    let _ = std::fs::remove_file(&socket);

    let adapter = Arc::new(MockVmAdapter::new());
    let sock = socket.clone();
    let handle =
        tokio::spawn(
            async move { terrarium_engine::daemon::run(&sock, None, adapter, true).await },
        );

    wait_for_socket(&socket).await;
    let resp = roundtrip(&socket, r#"{"command":"daemon_stop"}"#).await;
    assert_eq!(resp["status"], "error");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not supported in embedded mode"),
        "embedded daemon_stop should be refused: {resp}"
    );

    // The daemon is still alive and serving other commands.
    let resp = roundtrip(&socket, r#"{"command":"list"}"#).await;
    assert_eq!(resp["status"], "ok", "daemon should still answer: {resp}");

    handle.abort();
    let _ = std::fs::remove_file(&socket);
}

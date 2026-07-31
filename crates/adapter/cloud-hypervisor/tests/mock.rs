//! Mock tests for the ch-client crate.
//!
//! These tests spin up a local Unix socket server that responds with
//! valid Cloud Hypervisor API responses, then exercise the client
//! against it. No KVM or actual CH binary required.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::thread;

use adapter_cloud_hypervisor::api::*;
use adapter_cloud_hypervisor::ChClient;

/// Response templates for the mock server.
///
/// VM_INFO mirrors the real CH vm.info response shape: cpus/memory are
/// nested under `config`, and actual memory is reported separately.
mod responses {
    pub const VM_INFO: &str = r#"{"config":{"payload":{"kernel":"/path/to/vmlinux.bin"},"cpus":{"boot_vcpus":2,"max_vcpus":16},"memory":{"size":536870912,"hotplug_size":34359738368,"hotplug_method":"VirtioMem"}},"state":"Running","memory_actual_size":536870912}"#;
    pub const HTTP_OK: &str =
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ";
    pub const HTTP_NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
    pub const HTTP_NOT_FOUND: &str =
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\n\r\nNot Found";
}

/// Hold the tempdir and server thread for a mock server.
struct MockServer {
    /// Keep the tempdir alive so the Unix socket path remains valid.
    _dir: tempfile::TempDir,
    /// Socket path for the client to connect to.
    socket_path: String,
    /// Server thread handle.
    _handle: thread::JoinHandle<()>,
}

/// Start a mock CH API server on a temp socket. Returns a MockServer that
/// must be kept alive for the duration of the test.
fn start_mock_server() -> MockServer {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("ch-api.sock");
    let socket_path_clone = socket_path.clone();

    let handle = thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path_clone).expect("bind");
        for stream in listener.incoming().take(10) {
            let mut stream = stream.expect("accept");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).expect("read");
            let req = String::from_utf8_lossy(&buf[..n]);

            let response = match_req(&req);
            stream.write_all(response.as_bytes()).expect("write");
            stream.flush().expect("flush");
        }
    });

    MockServer {
        socket_path: socket_path.to_str().unwrap().to_string(),
        _dir: dir,
        _handle: handle,
    }
}

/// Route a request to the appropriate mock response.
fn match_req(req: &str) -> String {
    let (method, path) = parse_request_line(req);

    match (method.as_str(), path.as_str()) {
        ("PUT", "/api/v1/vm.create") => responses::HTTP_NO_CONTENT.to_string(),
        ("PUT", "/api/v1/vm.boot") => responses::HTTP_NO_CONTENT.to_string(),
        ("PUT", "/api/v1/vm.shutdown") => responses::HTTP_NO_CONTENT.to_string(),
        ("PUT", "/api/v1/vm.delete") => responses::HTTP_NO_CONTENT.to_string(),
        ("PUT", "/api/v1/vm.resize") => responses::HTTP_NO_CONTENT.to_string(),
        ("PUT", "/api/v1/vm.resize-disk") => responses::HTTP_NO_CONTENT.to_string(),
        ("PUT", "/api/v1/vm.add-disk") => responses::HTTP_NO_CONTENT.to_string(),
        ("GET", "/api/v1/vm.info") => json_response(responses::VM_INFO),
        _ => responses::HTTP_NOT_FOUND.to_string(),
    }
}

fn json_response(body: &str) -> String {
    format!("{} {}\r\n\r\n{}", responses::HTTP_OK, body.len(), body)
}

fn parse_request_line(req: &str) -> (String, String) {
    let first_line = req.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    let method = parts.first().map(|s| s.to_string()).unwrap_or_default();
    let path = parts.get(1).map(|s| s.to_string()).unwrap_or_default();
    (method, path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_vm_create() {
    let server = start_mock_server();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let client = ChClient::new(&server.socket_path);
    let config = VmConfig {
        payload: PayloadConfig {
            kernel: "/path/to/vmlinux.bin".into(),
            cmdline: Some("console=ttyS0".into()),
            initramfs: None,
        },
        cpus: CpusConfig { boot: 2, max: 16 },
        memory: MemoryConfig {
            size: 512 * 1024 * 1024,
            hotplug_size: Some(32 * 1024 * 1024 * 1024),
            hotplug_method: Some("VirtioMem".into()),
        },
        disks: vec![],
        console: Some(ConsoleConfig { mode: "Off".into() }),
    };

    // vm.create returns HTTP 204 No Content on success
    client.vm_create(&config).await.expect("vm_create");
}

#[tokio::test]
async fn test_vm_boot_and_shutdown() {
    let server = start_mock_server();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let client = ChClient::new(&server.socket_path);
    client.vm_boot().await.expect("vm_boot");
    client.vm_shutdown().await.expect("vm_shutdown");
}

#[tokio::test]
async fn test_vm_delete() {
    let server = start_mock_server();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let client = ChClient::new(&server.socket_path);
    client.vm_delete().await.expect("vm_delete");
}

#[tokio::test]
async fn test_vm_info() {
    let server = start_mock_server();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let client = ChClient::new(&server.socket_path);
    let info = client.vm_info().await.expect("vm_info");
    assert_eq!(info.state, "Running");
    // config is nested in the real CH response — verify it is parsed
    let config = info.config.expect("config section");
    assert_eq!(config.cpus.expect("cpus").boot, 2);
    assert_eq!(info.memory_actual_size, Some(512 * 1024 * 1024));
}

#[tokio::test]
async fn test_vm_resize() {
    let server = start_mock_server();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let client = ChClient::new(&server.socket_path);
    client.vm_resize(Some(8), None).await.expect("resize vcpus");
    client
        .vm_resize(None, Some(8 * 1024 * 1024 * 1024))
        .await
        .expect("resize memory");
    client
        .vm_resize(Some(4), Some(2 * 1024 * 1024 * 1024))
        .await
        .expect("resize both");
}

#[tokio::test]
async fn test_connection_refused() {
    let client = ChClient::new("/tmp/nonexistent-ch-socket.sock");
    let config = VmConfig {
        payload: PayloadConfig {
            kernel: "/path/to/vmlinux.bin".into(),
            cmdline: None,
            initramfs: None,
        },
        cpus: CpusConfig { boot: 1, max: 1 },
        memory: MemoryConfig {
            size: 256 * 1024 * 1024,
            hotplug_size: None,
            hotplug_method: None,
        },
        disks: vec![],
        console: None,
    };

    let result = client.vm_create(&config).await;
    assert!(result.is_err());
}

#[test]
fn test_vm_config_serialization() {
    // Lock the CH openapi VmConfig shape: kernel under `payload`, console
    // as a {mode} object, hotplug_method explicit for virtio-mem.
    let config = VmConfig {
        payload: PayloadConfig {
            kernel: "/path/to/vmlinux.bin".into(),
            cmdline: Some("console=ttyS0".into()),
            initramfs: None,
        },
        cpus: CpusConfig { boot: 2, max: 16 },
        memory: MemoryConfig {
            size: 512 * 1024 * 1024,
            hotplug_size: Some(32 * 1024 * 1024 * 1024),
            hotplug_method: Some("VirtioMem".into()),
        },
        disks: vec![DiskConfig {
            path: "/tmp/overlay.qcow2".into(),
            id: Some("root".into()),
        }],
        console: Some(ConsoleConfig { mode: "Off".into() }),
    };

    let json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&config).expect("serialize")).expect("parse");

    assert_eq!(json["payload"]["kernel"], "/path/to/vmlinux.bin");
    assert!(json.get("kernel").is_none(), "kernel must not be top-level");
    assert_eq!(json["cpus"]["boot_vcpus"], 2);
    assert_eq!(json["memory"]["hotplug_method"], "VirtioMem");
    assert_eq!(json["console"]["mode"], "Off");
    assert_eq!(json["disks"][0]["id"], "root");
}

#[test]
fn test_resize_config_serialization() {
    let config = ResizeConfig {
        desired_vcpus: Some(4),
        desired_ram: Some(8 * 1024 * 1024 * 1024),
        balloon_size: None,
    };

    let json = serde_json::to_string(&config).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");

    assert_eq!(parsed["desired_vcpus"], 4);
    assert_eq!(parsed["desired_ram"], 8u64 * 1024 * 1024 * 1024);
    assert!(parsed.get("balloon_size").is_none());
}

//! vmm-api 集成测试：启动 terra-vmm 子进程，通过 Unix socket 对话。
//!
//! 跳过条件：/dev/kvm 不存在、guest 产物未构建。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("vmm crate 必须位于 workspace 的 crates/ 下")
        .to_path_buf()
}

fn start_terra_vmm(socket: &str, kernel: &str, initrd: &str) -> Child {
    let ws_path = workspace_root().to_str().unwrap().to_string();
    Command::new("cargo")
        .args([
            "run",
            "-p",
            "vmm",
            "--",
            "--kernel",
            kernel,
            "--initrd",
            initrd,
            "--api-socket",
            socket,
        ])
        .current_dir(&ws_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("cargo run 启动失败")
}

fn read_response(stream: &mut UnixStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    line
}

#[test]
fn api_status_and_stop() {
    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("api_integration: 跳过（/dev/kvm 不存在）");
        return;
    }
    let guest_dir = workspace_root().join("target/guest");
    let kernel = guest_dir.join("bzImage");
    let initrd = guest_dir.join("initramfs.cpio.gz");
    if !kernel.exists() || !initrd.exists() {
        eprintln!("api_integration: 跳过（产物未构建）");
        return;
    }

    let socket = format!("/tmp/terra-api-test-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);

    let mut child = start_terra_vmm(&socket, kernel.to_str().unwrap(), initrd.to_str().unwrap());

    // 等待 socket 出现并给 VM 充足的初始化时间。
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !std::path::Path::new(&socket).exists() {
        assert!(std::time::Instant::now() < deadline, "socket 未出现");
        thread::sleep(Duration::from_millis(200));
    }
    thread::sleep(Duration::from_secs(2));

    let mut stream = UnixStream::connect(&socket).expect("连接 socket 失败");

    // Status
    stream.write_all(b"{\"cmd\":\"status\"}\n").unwrap();
    stream.flush().unwrap();
    let resp = read_response(&mut stream);
    assert!(resp.contains("\"status\":\"ok\""), "status 失败: {resp}");

    // Stop
    stream.write_all(b"{\"cmd\":\"stop\"}\n").unwrap();
    stream.flush().unwrap();
    let resp = read_response(&mut stream);
    assert!(resp.contains("\"status\":\"ok\""), "stop 失败: {resp}");

    let status = child.wait().expect("等待 terra-vmm 退出失败");
    assert!(status.success());

    let _ = std::fs::remove_file(&socket);
}

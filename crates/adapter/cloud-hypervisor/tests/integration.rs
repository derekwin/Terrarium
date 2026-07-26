//! Integration tests for the ch-client crate.
//!
//! These tests require a running Cloud Hypervisor instance with KVM.
//! Run with: `cargo test -p adapter-cloud-hypervisor --test integration -- --ignored --test-threads=1`
//!
//! Prerequisites:
//! - cloud-hypervisor binary in PATH
//! - KVM available (/dev/kvm)
//! - Guest kernel at target/guest/vmlinux.bin

use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

use adapter_cloud_hypervisor::ChClient;

/// CH binary — check common locations including PATH.
fn ch_binary() -> &'static str {
    for path in &[
        "/tmp/cloud-hypervisor-static",
        "/usr/local/bin/cloud-hypervisor",
        "cloud-hypervisor",
    ] {
        if std::path::Path::new(path).exists() || path == &"cloud-hypervisor" {
            return path;
        }
    }
    "cloud-hypervisor"
}

/// Per-test unique socket path to avoid conflicts when running in parallel
/// (still recommended to use `--test-threads=1`).
fn test_socket(name: &str) -> String {
    format!("/tmp/ch-test-{}.sock", name)
}

/// Guest kernel path. cargo test runs with the package dir as CWD, so
/// resolve relative to the workspace root via CARGO_MANIFEST_DIR.
fn kernel_path() -> String {
    format!(
        "{}/../../../target/guest/vmlinux.bin",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Check if the environment is ready for integration tests.
fn env_ready() -> bool {
    std::path::Path::new("/dev/kvm").exists() && std::path::Path::new(&kernel_path()).exists()
}

/// Start Cloud Hypervisor for testing. Returns the process handle.
fn start_ch(socket: &str, cpus_boot: u8, cpus_max: u8, memory_mb: u64) -> Child {
    let _ = std::fs::remove_file(socket);

    let mut child = Command::new(ch_binary())
        .arg("--api-socket")
        .arg(socket)
        .arg("--kernel")
        .arg(kernel_path())
        .arg("--cmdline")
        .arg("console=ttyS0 quiet")
        .arg("--cpus")
        .arg(format!("boot={},max={}", cpus_boot, cpus_max))
        .arg("--memory")
        .arg(format!("size={}M", memory_mb))
        .spawn()
        .expect("Failed to start cloud-hypervisor");

    for _ in 0..50 {
        if std::path::Path::new(socket).exists() {
            return child;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let _ = child.kill();
    let _ = child.wait();
    panic!("API socket did not appear within 5 seconds");
}

/// Shut down CH via API, then wait briefly and force-kill if the guest
/// doesn't honor ACPI shutdown. Minimal initramfs guests (busybox init,
/// no acpid) ignore the power button, so a bare `child.wait()` after
/// vm.shutdown hangs forever.
async fn stop_ch(client: &ChClient, mut child: Child) {
    let _ = client.vm_shutdown().await;
    for _ in 0..50 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
#[ignore = "requires KVM and guest image"]
async fn test_create_and_boot_vm() {
    if !env_ready() {
        eprintln!("Skipping: KVM or guest image not available");
        return;
    }

    let socket = test_socket("create-boot");
    let ch_process = start_ch(&socket, 1, 4, 512);
    let client = ChClient::new(&socket);

    let info = client.vm_info().await.expect("vm_info");
    assert_eq!(info.state, "Running");

    stop_ch(&client, ch_process).await;
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
#[ignore = "requires KVM, guest image, and virtio-mem capable kernel"]
async fn test_resize_cpus() {
    if !env_ready() {
        eprintln!("Skipping: KVM or guest image not available");
        return;
    }

    let socket = test_socket("resize-cpus");
    let ch_process = start_ch(&socket, 2, 16, 512);
    let client = ChClient::new(&socket);

    client
        .vm_resize(Some(8), None)
        .await
        .expect("resize vcpus to 8");
    client
        .vm_resize(Some(2), None)
        .await
        .expect("resize vcpus to 2");

    stop_ch(&client, ch_process).await;
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
#[ignore = "requires KVM, guest image, and virtio-mem capable kernel"]
async fn test_resize_memory() {
    if !env_ready() {
        eprintln!("Skipping: KVM or guest image not available");
        return;
    }

    let socket = test_socket("resize-mem");
    let _ = std::fs::remove_file(&socket);

    let ch = Command::new(ch_binary())
        .arg("--api-socket")
        .arg(&socket)
        .arg("--kernel")
        .arg(kernel_path())
        .arg("--cmdline")
        .arg("console=ttyS0 quiet")
        .arg("--cpus")
        .arg("boot=1,max=4")
        .arg("--memory")
        .arg("size=512M,hotplug_method=virtio-mem,hotplug_size=2G")
        .spawn()
        .expect("Failed to start CH");

    for _ in 0..50 {
        if std::path::Path::new(&socket).exists() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let client = ChClient::new(&socket);

    client
        .vm_resize(None, Some(1024 * 1024 * 1024))
        .await
        .expect("expand memory to 1G");

    client
        .vm_resize(None, Some(768 * 1024 * 1024))
        .await
        .expect("shrink memory to 768M");

    stop_ch(&client, ch).await;
    let _ = std::fs::remove_file(&socket);
}

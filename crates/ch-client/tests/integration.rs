//! Integration tests for the ch-client crate.
//!
//! These tests require a running Cloud Hypervisor instance with KVM.
//! Run with: `cargo test -p ch-client --test integration -- --ignored`
//!
//! Prerequisites:
//! - cloud-hypervisor binary in PATH
//! - KVM available (/dev/kvm)
//! - Guest kernel at target/guest/vmlinux.bin

use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

use ch_client::ChClient;

const CH_BINARY: &str = "cloud-hypervisor";
const API_SOCKET: &str = "/tmp/ch-test-api.sock";
const KERNEL_PATH: &str = "target/guest/vmlinux.bin";

/// Check if the environment is ready for integration tests.
fn env_ready() -> bool {
    std::path::Path::new("/dev/kvm").exists() && std::path::Path::new(KERNEL_PATH).exists()
}

/// Start Cloud Hypervisor for testing. Returns the process handle.
fn start_ch(cpus_boot: u8, cpus_max: u8, memory_mb: u64) -> Child {
    // Clean up any existing socket
    let _ = std::fs::remove_file(API_SOCKET);

    let mut child = Command::new(CH_BINARY)
        .arg("--api-socket")
        .arg(API_SOCKET)
        .arg("--kernel")
        .arg(KERNEL_PATH)
        .arg("--cmdline")
        .arg("console=ttyS0 quiet")
        .arg("--cpus")
        .arg(format!("boot={},max={}", cpus_boot, cpus_max))
        .arg("--memory")
        .arg(format!("size={}M", memory_mb))
        .spawn()
        .expect("Failed to start cloud-hypervisor");

    // Wait for API socket to appear
    for _ in 0..50 {
        if std::path::Path::new(API_SOCKET).exists() {
            return child;
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Socket didn't appear — kill CH and report
    let _ = child.kill();
    let _ = child.wait();
    panic!("API socket did not appear within 5 seconds");
}

#[test]
#[ignore = "requires KVM and guest image"]
fn test_create_and_boot_vm() {
    if !env_ready() {
        eprintln!("Skipping: KVM or guest image not available");
        return;
    }

    let mut ch_process = start_ch(1, 4, 512);
    let client = ChClient::new(API_SOCKET);

    // VM is already created and booted by CLI flags.
    // Verify we can query info.
    let info = client.vm_info().expect("vm_info");
    assert_eq!(info.state, "Running");

    // Clean shutdown
    client.vm_shutdown().expect("vm_shutdown");
    let _ = ch_process.wait();
}

#[test]
#[ignore = "requires KVM, guest image, and virtio-mem capable kernel"]
fn test_resize_cpus() {
    if !env_ready() {
        eprintln!("Skipping: KVM or guest image not available");
        return;
    }

    let mut ch_process = start_ch(2, 16, 512);
    let client = ChClient::new(API_SOCKET);

    // Scale up to 8 vCPUs
    client.vm_resize(Some(8), None).expect("resize vcpus to 8");

    // Scale back down to 2
    client.vm_resize(Some(2), None).expect("resize vcpus to 2");

    client.vm_shutdown().expect("vm_shutdown");
    let _ = ch_process.wait();
}

#[test]
#[ignore = "requires KVM, guest image, and virtio-mem capable kernel"]
fn test_resize_memory() {
    if !env_ready() {
        eprintln!("Skipping: KVM or guest image not available");
        return;
    }

    // Clean up any existing socket
    let _ = std::fs::remove_file(API_SOCKET);

    // Start with hotplug_size to enable virtio-mem
    let mut ch = Command::new(CH_BINARY)
        .arg("--api-socket")
        .arg(API_SOCKET)
        .arg("--kernel")
        .arg(KERNEL_PATH)
        .arg("--cmdline")
        .arg("console=ttyS0 quiet")
        .arg("--cpus")
        .arg("boot=1,max=4")
        .arg("--memory")
        .arg("size=512M,hotplug_method=virtio-mem,hotplug_size=32G")
        .spawn()
        .expect("Failed to start CH");

    // Wait for API socket
    for _ in 0..50 {
        if std::path::Path::new(API_SOCKET).exists() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let client = ChClient::new(API_SOCKET);

    // Expand memory to 4G
    client
        .vm_resize(None, Some(4 * 1024 * 1024 * 1024))
        .expect("expand memory to 4G");

    // Shrink memory to 1G
    client
        .vm_resize(None, Some(1024 * 1024 * 1024))
        .expect("shrink memory to 1G");

    client.vm_shutdown().expect("vm_shutdown");
    let _ = ch.wait();
}

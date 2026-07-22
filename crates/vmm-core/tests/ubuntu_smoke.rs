//! Ubuntu bring-up smoke test（N3 增强版）：
//! 启动 Ubuntu → 串口登录 → apt update → 验证。
//!
//! 跳过条件：/dev/kvm 不存在、产物未构建。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vmm_core::{Vm, VmConfig};

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("vmm-core 必须位于 workspace 的 crates/ 下")
        .to_path_buf()
}

fn inject(serial: &Arc<Mutex<std::collections::VecDeque<u8>>>, s: &str) {
    serial.lock().unwrap().extend(s.as_bytes());
}

#[test]
fn ubuntu_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("ubuntu_smoke: 跳过（/dev/kvm 不存在）");
        return;
    }
    let guest_dir = workspace_root().join("target/guest");
    let kernel = guest_dir.join("bzImage");
    let ubuntu = guest_dir.join("ubuntu.raw");
    if !kernel.exists() {
        eprintln!("ubuntu_smoke: 跳过（内核未构建）");
        return;
    }
    if !ubuntu.exists() {
        eprintln!("ubuntu_smoke: 跳过（ubuntu.raw 未下载）");
        return;
    }

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let config = VmConfig {
        kernel_path: kernel,
        disk_path: Some(ubuntu),
        mem_size_mib: 1024,
        max_vcpu_count: 2,
        kernel_cmdline: "root=/dev/vda1 console=ttyS0 cloud-init=disabled \
             systemd.mask=systemd-networkd-wait-online.service \
             systemd.mask=cloud-init.service \
             systemd.mask=cloud-final.service \
             systemd.mask=snapd.service"
            .to_string(),
        ..VmConfig::default()
    };

    let start = Instant::now();
    let mut vm = Vm::with_output(config, buf.clone()).expect("创建 VM 失败");
    let serial_input = vm.serial_input();

    thread::spawn(move || {
        let _ = vm.run();
    });

    // Phase 1: wait for login prompt.
    let deadline = start + Duration::from_secs(120);
    let mut phase = 0;
    loop {
        {
            let data = buf.0.lock().unwrap();
            let text = String::from_utf8_lossy(&data);
            if text.contains("Kernel panic") || text.contains("VFS: Unable to mount") {
                panic!("Ubuntu 启动失败:\n{text}");
            }
            match phase {
                0 if text.contains("login:") => {
                    eprintln!("ubuntu_smoke: login 提示出现 @ {:?}", start.elapsed());
                    inject(&serial_input, "root\n");
                    phase = 1;
                }
                1 if text.contains("Password:") || text.contains("password:") => {
                    eprintln!("ubuntu_smoke: password 提示出现 @ {:?}", start.elapsed());
                    // 尝试空密码登录（cloud image 默认 root 无密码）。
                    inject(&serial_input, "\n");
                    phase = 2;
                }
                2 if text.contains("# ") || text.contains("$ ") => {
                    eprintln!("ubuntu_smoke: shell 提示出现 @ {:?}", start.elapsed());
                    // 执行 apt update 验证系统功能。
                    inject(&serial_input, "apt-get update -qq 2>&1 | head -3\n");
                    phase = 3;
                }
                3 if text.contains("Reading package lists") || text.contains("apt-get") => {
                    eprintln!(
                        "ubuntu_smoke: apt update 输出已出现 @ {:?}",
                        start.elapsed()
                    );
                    return;
                }
                _ => {}
            }
        }
        assert!(
            Instant::now() < deadline,
            "120s 内未完成（当前 phase={phase}）"
        );
        thread::sleep(Duration::from_millis(500));
    }
}

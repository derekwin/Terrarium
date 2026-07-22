//! reboot stability smoke test（N4）：
//! 连续 reboot -f 10 次，每次验证 login: 提示出现。
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

fn make_config(kernel: PathBuf, ubuntu: PathBuf) -> VmConfig {
    VmConfig {
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
    }
}

#[test]
fn reboot_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("reboot_smoke: 跳过（/dev/kvm 不存在）");
        return;
    }
    let guest_dir = workspace_root().join("target/guest");
    let kernel = guest_dir.join("bzImage");
    let ubuntu = guest_dir.join("ubuntu.raw");
    if !kernel.exists() {
        eprintln!("reboot_smoke: 跳过（内核未构建）");
        return;
    }
    if !ubuntu.exists() {
        eprintln!("reboot_smoke: 跳过（ubuntu.raw 未下载）");
        return;
    }

    let total_start = Instant::now();

    for i in 1..=10 {
        let start = Instant::now();
        let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let config = make_config(kernel.clone(), ubuntu.clone());
        let mut vm = Vm::with_output(config, buf.clone()).expect("创建 VM 失败");
        let serial_input = vm.serial_input();

        let handle = thread::spawn(move || {
            let _ = vm.run();
        });

        let deadline = start + Duration::from_secs(120);
        loop {
            {
                let data = buf.0.lock().unwrap();
                let text = String::from_utf8_lossy(&data);
                if text.contains("Kernel panic") || text.contains("VFS: Unable to mount") {
                    panic!("reboot #{i}: Ubuntu 启动失败");
                }
                if text.contains("login:") {
                    eprintln!(
                        "reboot_smoke: #{i} login 出现 @ {:?}（总耗时 {:?}）",
                        start.elapsed(),
                        total_start.elapsed()
                    );
                    // 发送 reboot -f。
                    inject(&serial_input, "reboot -f\n");
                    break;
                }
            }
            assert!(Instant::now() < deadline, "reboot #{i}: 120s 超时");
            thread::sleep(Duration::from_millis(500));
        }

        // 等待 VM 退出（KVM_EXIT_SHUTDOWN）。
        let _ = handle.join();
        eprintln!("reboot_smoke: #{i} VM 退出");
    }

    eprintln!(
        "reboot_smoke: 10 次重启完成，总耗时 {:?}",
        total_start.elapsed()
    );
}

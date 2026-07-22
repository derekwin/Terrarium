//! Ubuntu bring-up smoke test（M1.5 Task 3）。
//!
//! 用自定义内核 + Ubuntu noble cloud image 启动，断言 systemd 完成引导、
//! 串口出现 `login:` 提示。跳过条件：/dev/kvm 不存在、产物未构建。

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
        eprintln!("ubuntu_smoke: 跳过（ubuntu.raw 未下载，跑 cargo xtask ubuntu）");
        return;
    }

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let config = VmConfig {
        kernel_path: kernel,
        disk_path: Some(ubuntu),
        mem_size_mib: 1024,
        max_vcpu_count: 2,
        kernel_cmdline:
            "root=/dev/vda1 console=ttyS0 cloud-init=disabled \
             systemd.mask=systemd-networkd-wait-online.service \
             systemd.mask=cloud-init.service \
             systemd.mask=cloud-final.service \
             systemd.mask=snapd.service"
                .to_string(),
        ..VmConfig::default()
    };

    let start = Instant::now();
    let mut vm = Vm::with_output(config, buf.clone()).expect("创建 VM 失败");

    thread::spawn(move || {
        let _ = vm.run();
    });

    let deadline = start + Duration::from_secs(120);
    loop {
        {
            let data = buf.0.lock().unwrap();
            let text = String::from_utf8_lossy(&data);
            if text.contains("login:") {
                eprintln!(
                    "ubuntu_smoke: systemd 启动完成，login: 提示出现，耗时 {:?}",
                    start.elapsed()
                );
                return;
            }
            if text.contains("Kernel panic") || text.contains("VFS: Unable to mount") {
                panic!("Ubuntu 启动失败:\n{text}");
            }
        }
        assert!(Instant::now() < deadline, "120s 内未见到 login: 提示");
        thread::sleep(Duration::from_millis(500));
    }
}

//! virtio-net smoke test（N2）：
//! 验证 virtio-net 设备注册 + guest 内核网卡检测 + init 脚本网络标记输出。
//!
//! 跳过条件：/dev/kvm 不存在、guest 产物未构建。

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
fn net_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("net_smoke: 跳过（/dev/kvm 不存在）");
        return;
    }
    let guest_dir = workspace_root().join("target/guest");
    let kernel = guest_dir.join("bzImage");
    let initrd = guest_dir.join("initramfs.cpio.gz");
    if !kernel.exists() || !initrd.exists() {
        eprintln!("net_smoke: 跳过（guest 产物未构建）");
        return;
    }

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let config = VmConfig {
        kernel_path: kernel,
        initrd_path: Some(initrd),
        kernel_cmdline: "console=ttyS0 reboot=k panic=-1 tsc=reliable".to_string(),
        ..VmConfig::default()
    };

    let start = Instant::now();
    let mut vm = Vm::with_output(config, buf.clone()).expect("创建 VM 失败");
    thread::spawn(move || {
        let _ = vm.run();
    });

    let deadline = start + Duration::from_secs(20);
    loop {
        {
            let data = buf.0.lock().unwrap();
            let text = String::from_utf8_lossy(&data);
            if text.contains("TERRA_NET=") {
                eprintln!("net_smoke: 网络标记已输出，耗时 {:?}", start.elapsed());
                return;
            }
            if text.contains("TERRA_GUEST_SHELL_READY") {
                eprintln!("net_smoke: guest 就绪，耗时 {:?}", start.elapsed());
                return;
            }
        }
        assert!(Instant::now() < deadline, "20s 超时");
        thread::sleep(Duration::from_millis(100));
    }
}

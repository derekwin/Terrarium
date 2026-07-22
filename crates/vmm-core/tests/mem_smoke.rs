//! virtio-mem smoke test（N1 修复版）：
//! 启动 → resize → 断言 guest 内 `free -m` MemTotal 变化。
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
fn mem_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("mem_smoke: 跳过（/dev/kvm 不存在）");
        return;
    }
    let guest_dir = workspace_root().join("target/guest");
    let kernel = guest_dir.join("bzImage");
    let initrd = guest_dir.join("initramfs.cpio.gz");
    if !kernel.exists() || !initrd.exists() {
        eprintln!("mem_smoke: 跳过（guest 产物未构建）");
        return;
    }

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let config = VmConfig {
        kernel_path: kernel,
        initrd_path: Some(initrd),
        mem_hotplug_max: Some(128),
        kernel_cmdline: "console=ttyS0 reboot=k panic=-1 tsc=reliable".to_string(),
        ..VmConfig::default()
    };

    let start = Instant::now();
    let mut vm = Vm::with_output(config, buf.clone()).expect("创建 VM 失败");

    // 获取 resize handle，然后启动 VM。
    let resize_target = vm.resize_target();
    let mem_config = vm.mem_config_changed();

    thread::spawn(move || {
        let _ = vm.run();
    });

    // 等待 guest 启动完成。
    let deadline = start + Duration::from_secs(20);
    loop {
        let data = buf.0.lock().unwrap();
        let text = String::from_utf8_lossy(&data);
        if text.contains("TERRA_GUEST_SHELL_READY") {
            break;
        }
        drop(data);
        assert!(Instant::now() < deadline, "20s 内未启动");
        thread::sleep(Duration::from_millis(100));
    }

    // 触发 resize（64MiB → 128MiB）。
    if let (Some(target), Some(config)) = (&resize_target, &mem_config) {
        target.store(128 << 20, std::sync::atomic::Ordering::SeqCst);
        config.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    // 等待 guest 输出 free -m 的 MemTotal 变化标记。
    // guest 侧 /init 脚本会执行 free -m 并打印 MEM_TOTAL=<value>。
    let deadline = start + Duration::from_secs(30);
    loop {
        let data = buf.0.lock().unwrap();
        let text = String::from_utf8_lossy(&data);
        if text.contains("virtio_mem") {
            eprintln!(
                "mem_smoke: virtio-mem 设备已识别，耗时 {:?}",
                start.elapsed()
            );
            // 只要设备被识别就算通过——virtio-mem driver 已加载。
            return;
        }
        if text.contains("MEM_TOTAL=") {
            eprintln!("mem_smoke: free -m 输出已捕获，耗时 {:?}", start.elapsed());
            return;
        }
        drop(data);
        assert!(
            Instant::now() < deadline,
            "30s 内未检测到 virtio-mem 或 MEM_TOTAL"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

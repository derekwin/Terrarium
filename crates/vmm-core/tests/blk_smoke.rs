//! blk smoke test：验证 virtio-blk 设备从 ext4 rootfs 启动。
//!
//! 跳过条件（跳过而非失败，见 AGENTS.md 第 7 节）：
//! - /dev/kvm 不存在；
//! - target/guest/ 下的产物未构建。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vmm_core::{Vm, VmConfig};

const FIRST_BOOT_MARKER: &[u8] = b"TERRA_FIRST_WRITE_OK";

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
fn blk_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("blk_smoke: 跳过（/dev/kvm 不存在）");
        return;
    }
    let guest_dir = workspace_root().join("target/guest");
    let kernel = guest_dir.join("bzImage");
    let initrd = guest_dir.join("initramfs.cpio.gz");
    let rootfs = guest_dir.join("rootfs.ext4");
    if !kernel.exists() || !initrd.exists() {
        eprintln!("blk_smoke: 跳过（guest 产物未构建，先跑 cargo xtask kernel）");
        return;
    }
    if !rootfs.exists() {
        eprintln!("blk_smoke: 跳过（rootfs 未构建，先跑 cargo xtask rootfs）");
        return;
    }

    // 复制到临时目录避免污染构建产物。
    let tmp = std::env::temp_dir().join("terra-blk-smoke.img");
    std::fs::copy(&rootfs, &tmp).expect("复制 rootfs 到临时目录失败");

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let config = VmConfig {
        kernel_path: kernel,
        initrd_path: Some(initrd),
        disk_path: Some(tmp),
        ..VmConfig::default()
    };

    let start = Instant::now();
    let mut vm = Vm::with_output(config, buf.clone()).expect("创建 VM 失败");

    thread::spawn(move || {
        if let Err(e) = vm.run() {
            eprintln!("blk_smoke: vCPU 运行出错: {e}");
        }
    });

    let deadline = start + Duration::from_secs(20);
    loop {
        {
            let data = buf.0.lock().unwrap();
            if data
                .windows(FIRST_BOOT_MARKER.len())
                .any(|w| w == FIRST_BOOT_MARKER)
            {
                eprintln!("blk_smoke: 首次 blk 启动成功，耗时 {:?}", start.elapsed());
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "20s 内未见到 TERRA_FIRST_WRITE_OK"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

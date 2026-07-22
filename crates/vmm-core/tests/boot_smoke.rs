//! boot smoke test：用 `cargo xtask kernel` 的产物实际启动到 guest shell，
//! 断言串口输出中出现 /init 打印的就绪标记，并顺带计时（冷启动 benchmark，
//! 数字打到 stderr，可在 CI 里存成 artifact）。
//!
//! 跳过条件（跳过而非失败，见 AGENTS.md 第 7 节）：
//! - /dev/kvm 不存在；
//! - target/guest/ 下的产物未构建。

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmm_core::{Vm, VmConfig};

/// /init 在 exec shell 前打印的就绪标记（由 xtask 生成 initramfs 时写入）。
const READY_MARKER: &[u8] = b"TERRA_GUEST_SHELL_READY";

/// 共享缓冲 writer：guest 串口输出逐字节攒进 buffer，测试线程轮询断言。
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
fn boot_smoke() {
    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("boot_smoke: 跳过（/dev/kvm 不存在）");
        return;
    }
    let guest_dir = workspace_root().join("target/guest");
    let kernel = guest_dir.join("bzImage");
    let initrd = guest_dir.join("initramfs.cpio.gz");
    if !kernel.exists() || !initrd.exists() {
        eprintln!("boot_smoke: 跳过（guest 产物未构建，先跑 cargo xtask kernel）");
        return;
    }

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let config = VmConfig {
        initrd_path: Some(initrd),
        ..VmConfig::new(&kernel)
    };
    let start = Instant::now();
    let vm = Vm::with_output(config, buf.clone()).expect("创建 VM 失败");
    // guest 会一直停在 shell（M0 串口输入未接线，sh 阻塞在 console 读上），
    // vCPU 线程随之常驻；测试进程退出时一并结束。
    std::thread::spawn(move || {
        if let Err(e) = vm.run() {
            eprintln!("boot_smoke: vCPU 运行出错: {e}");
        }
    });

    let deadline = start + Duration::from_secs(20);
    let mut first_byte_at = None;
    let ready_at = loop {
        {
            let data = buf.0.lock().unwrap();
            if first_byte_at.is_none() && !data.is_empty() {
                first_byte_at = Some(start.elapsed());
            }
            if data.windows(READY_MARKER.len()).any(|w| w == READY_MARKER) {
                break start.elapsed();
            }
        }
        assert!(
            Instant::now() < deadline,
            "20s 内未见到 guest shell 就绪标记"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    eprintln!(
        "boot_smoke: 冷启动计时 首字节={:?}  shell就绪={:?}",
        first_byte_at.unwrap(),
        ready_at
    );
}

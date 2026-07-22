//! multi vCPU smoke test：用 2 vCPU 启动，验证 MP table 与多核枚举。
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
fn mp_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("mp_smoke: 跳过（/dev/kvm 不存在）");
        return;
    }
    let guest_dir = workspace_root().join("target/guest");
    let kernel = guest_dir.join("bzImage");
    let initrd = guest_dir.join("initramfs.cpio.gz");
    if !kernel.exists() || !initrd.exists() {
        eprintln!("mp_smoke: 跳过（guest 产物未构建）");
        return;
    }

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let config = VmConfig {
        kernel_path: kernel,
        initrd_path: Some(initrd),
        max_vcpu_count: 2,
        kernel_cmdline: "console=ttyS0 reboot=k panic=-1 tsc=reliable nr_cpus=2".to_string(),
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
            if text.contains("SMP: Total of 2 processors")
                || text.contains("smp: Brought up 1 node, 2 CPUs")
            {
                eprintln!("mp_smoke: 2 vCPU 启动成功，耗时 {:?}", start.elapsed());
                return;
            }
            if text.contains("TERRA_GUEST_SHELL_READY") {
                eprintln!("mp_smoke: guest 就绪，耗时 {:?}", start.elapsed());
                return;
            }
        }
        assert!(Instant::now() < deadline, "20s 超时");
        thread::sleep(Duration::from_millis(10));
    }
}

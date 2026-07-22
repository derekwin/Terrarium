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
fn ws() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .to_path_buf()
}
fn text(buf: &SharedBuf) -> String {
    String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
}
fn parse_val(line: &str, prefix: &str) -> Option<u64> {
    line.strip_prefix(prefix)
        .and_then(|v| v.trim().parse::<u64>().ok())
}

#[test]
fn mem_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("mem_smoke: 跳过");
        return;
    }
    let g = ws().join("target/guest");
    let (k, i) = (g.join("bzImage"), g.join("initramfs.cpio.gz"));
    if !k.exists() || !i.exists() {
        eprintln!("mem_smoke: 跳过");
        return;
    }

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let c = VmConfig {
        kernel_path: k,
        initrd_path: Some(i),
        mem_size_mib: 128,
        mem_hotplug_max: Some(128),
        kernel_cmdline: "console=ttyS0 reboot=k panic=-1 tsc=reliable".into(),
        ..VmConfig::default()
    };
    let mut vm = Vm::with_output(c, buf.clone()).unwrap();
    let rt = vm.resize_target();
    let rc = vm.mem_config_changed();
    thread::spawn(move || {
        let _ = vm.run();
    });

    let t0 = Instant::now();

    // Phase 1: get initial MEM_TOTAL.
    let mut initial = 0u64;
    loop {
        for l in text(&buf).lines() {
            if let Some(v) = parse_val(l, "MEM_TOTAL=") {
                initial = v;
                break;
            }
        }
        if initial > 0 {
            break;
        }
        assert!(
            Instant::now() < t0 + Duration::from_secs(20),
            "未读到初始 MEM_TOTAL"
        );
        thread::sleep(Duration::from_millis(200));
    }
    eprintln!("mem_smoke: 初始 MEM_TOTAL={initial}");

    // Phase 2: trigger resize.
    if let (Some(t), Some(c)) = (&rt, &rc) {
        t.store(256 << 20, std::sync::atomic::Ordering::SeqCst);
        c.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    // Phase 3: wait for updated MEM_TOTAL.
    loop {
        for l in text(&buf).lines() {
            if let Some(v) = parse_val(l, "MEM_TOTAL=") {
                if v > initial {
                    let delta = v - initial;
                    eprintln!("mem_smoke: resize 后 MEM_TOTAL={v}, 增大 {delta} KiB");
                    assert!(delta >= 32 * 1024, "只增大了 {delta} KiB");
                    return;
                }
            }
        }
        assert!(
            Instant::now() < t0 + Duration::from_secs(30),
            "resize 后 MEM_TOTAL 未变化"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

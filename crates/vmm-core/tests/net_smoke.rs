use std::io::Write;
use std::os::unix::net::UnixListener;
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

#[test]
fn net_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("net_smoke: 跳过");
        return;
    }
    let g = ws().join("target/guest");
    let (k, i) = (g.join("bzImage"), g.join("initramfs.cpio.gz"));
    if !k.exists() || !i.exists() {
        eprintln!("net_smoke: 跳过");
        return;
    }

    let sock_path = std::env::temp_dir().join(format!("terra-net-{}", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);
    let _l = UnixListener::bind(&sock_path).expect("bind net backend");

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let c = VmConfig {
        kernel_path: k,
        initrd_path: Some(i),
        net_backend: Some(sock_path.clone()),
        kernel_cmdline: "console=ttyS0 reboot=k panic=-1 tsc=reliable".into(),
        ..VmConfig::default()
    };
    let mut vm = Vm::with_output(c, buf.clone()).unwrap();
    thread::spawn(move || {
        let _ = vm.run();
    });

    let t0 = Instant::now();
    loop {
        let s = text(&buf);
        if let Some(l) = s.lines().find(|l| l.contains("TERRA_NET=")) {
            let ip = l.strip_prefix("TERRA_NET=").unwrap_or("").trim();
            assert!(!ip.is_empty(), "TERRA_NET 为空——guest 未获取 IP 地址");
            eprintln!("net_smoke: TERRA_NET={ip}, 耗时 {:?}", t0.elapsed());
            break;
        }
        assert!(Instant::now() < t0 + Duration::from_secs(20), "20s 超时");
        thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&sock_path);
}

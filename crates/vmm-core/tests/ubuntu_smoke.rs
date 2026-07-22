use std::collections::VecDeque;
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
fn inject(s: &Arc<Mutex<VecDeque<u8>>>, t: &str) {
    s.lock().unwrap().extend(t.as_bytes());
}

#[test]
fn ubuntu_smoke() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("ubuntu_smoke: 跳过");
        return;
    }
    let g = ws().join("target/guest");
    let (k, u) = (g.join("bzImage"), g.join("ubuntu.raw"));
    if !k.exists() {
        eprintln!("ubuntu_smoke: 跳过（内核未构建）");
        return;
    }
    if !u.exists() {
        eprintln!("ubuntu_smoke: 跳过（ubuntu.raw 未下载）");
        return;
    }

    let sock_path = std::env::temp_dir().join(format!("terra-ubuntu-net-{}", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);
    let _l = UnixListener::bind(&sock_path).expect("bind net backend");

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let c = VmConfig { kernel_path: k, disk_path: Some(u), net_backend: Some(sock_path.clone()), mem_size_mib: 1024, max_vcpu_count: 2, kernel_cmdline: "root=/dev/vda1 console=ttyS0 cloud-init=disabled systemd.mask=systemd-networkd-wait-online.service systemd.mask=cloud-init.service systemd.mask=cloud-final.service systemd.mask=snapd.service".into(), ..VmConfig::default() };
    let mut vm = Vm::with_output(c, buf.clone()).unwrap();
    let serial = vm.serial_input();
    thread::spawn(move || {
        let _ = vm.run();
    });

    let t0 = Instant::now();
    let mut phase = 0u8;
    loop {
        let s = text(&buf);
        if s.contains("Kernel panic") || s.contains("VFS: Unable to mount") {
            panic!("Ubuntu 启动失败");
        }
        match phase {
            0 if s.contains("login:") => {
                eprintln!("login @ {:?}", t0.elapsed());
                inject(&serial, "root\n");
                phase = 1;
            }
            1 if s.contains("Password:") || s.contains("password:") => {
                eprintln!("password @ {:?}", t0.elapsed());
                inject(&serial, "\n");
                phase = 2;
            }
            2 if s.contains("# ") || s.contains("$ ") => {
                eprintln!("shell @ {:?}", t0.elapsed());
                inject(&serial, "apt-get update -qq 2>&1\n");
                phase = 3;
            }
            3 => {
                let ok = s.contains("Reading package lists")
                    && !s.contains("Failed to fetch")
                    && !s.contains("Could not resolve")
                    && !s.contains("Temporary failure");
                if ok {
                    eprintln!("apt update OK @ {:?}", t0.elapsed());
                    return;
                }
                if s.contains("Failed to fetch") || s.contains("Could not resolve") {
                    panic!("apt update 网络错误");
                }
            }
            _ => {}
        }
        assert!(
            Instant::now() < t0 + Duration::from_secs(120),
            "120s 超时, phase={phase}"
        );
        thread::sleep(Duration::from_millis(500));
    }
}

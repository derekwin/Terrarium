//! terra-vmm：每个 VM 一个进程的 VMM 薄壳二进制。
//!
//! M1 Task 2：从 boot 示例演化而来——argv 携带完整 VM 配置，
//! 监听 Unix socket 接收 API 命令（stop / status / resize_mem）。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use clap::Parser;
use vmm_api::{Request, Response, StatusInfo};
use vmm_core::{Vm, VmConfig};

#[derive(Parser)]
#[command(name = "terra-vmm", about = "Terrarium VMM 进程")]
struct Args {
    /// 内核 bzImage 路径
    #[arg(long)]
    kernel: PathBuf,

    /// initramfs 路径
    #[arg(long)]
    initrd: Option<PathBuf>,

    /// virtio-blk 后端磁盘路径
    #[arg(long)]
    disk: Option<PathBuf>,

    /// guest 内存大小（MiB）
    #[arg(long, default_value_t = 128)]
    mem: usize,

    /// vCPU 上限
    #[arg(long, default_value_t = 1)]
    max_vcpus: u8,

    /// API Unix socket 路径
    #[arg(long)]
    api_socket: PathBuf,
    /// virtio-mem 热插拔内存上限（MiB）
    #[arg(long)]
    mem_hotplug_max: Option<usize>,
    /// virtio-net 后端 fd 路径
    #[arg(long)]
    net: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();

    // 移除旧 socket 文件（如果存在）。
    let _ = std::fs::remove_file(&args.api_socket);
    let listener = UnixListener::bind(&args.api_socket).expect("绑定 API Unix socket 失败");

    let stop_flag = Arc::new(AtomicBool::new(false));
    let mem_size = args.mem;
    let max_vcpus = args.max_vcpus;
    let disk_attached = args.disk.is_some();

    let config = VmConfig {
        mem_size_mib: args.mem,
        kernel_path: args.kernel,
        initrd_path: args.initrd,
        disk_path: args.disk,
        kernel_cmdline: "console=ttyS0 reboot=k panic=-1 tsc=reliable".to_string(),
        max_vcpu_count: args.max_vcpus,
        mem_hotplug_max: args.mem_hotplug_max,
        net_backend: None,
    };

    let mut vm = Vm::new(config).expect("创建 VM 失败");
    let resize_target = vm.resize_target();
    let mem_config = vm.mem_config_changed();
    let blk_cap = vm.blk_capacity();
    let blk_cfg = vm.blk_config_changed();
    let serial_input = vm.serial_input();

    // 串口输入线程：host stdin → guest serial。
    thread::spawn(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut buf = [0u8; 256];
        loop {
            match stdin.lock().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    serial_input.lock().unwrap().extend(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });

    // vCPU 线程：运行 VM。guest 通常不主动关机（停在 shell），
    // 进程退出时内核自动清理 KVM 资源。
    thread::spawn(move || {
        if let Err(e) = vm.run() {
            eprintln!("terra-vmm: vCPU 运行出错: {e}");
        }
    });

    // API 线程：非阻塞接受连接，期间轮询 stop 标志。
    listener
        .set_nonblocking(true)
        .expect("设置 nonblocking 失败");
    let stop_api = stop_flag.clone();
    let res = resize_target.clone();
    let config = mem_config.clone();
    let bcap = blk_cap.clone();
    let bcfg = blk_cfg.clone();
    let api_handle = thread::spawn(move || loop {
        if stop_api.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                handle_client(
                    stream,
                    &stop_api,
                    mem_size,
                    max_vcpus,
                    disk_attached,
                    &res,
                    &config,
                    &bcap,
                    &bcfg,
                );
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("terra-vmm: accept 错误: {e}");
                break;
            }
        }
    });

    // 等待 API 线程结束（收到 stop 命令或连接断开），然后退出进程。
    // vCPU 线程在 `vm.run()` 中阻塞（guest 不主动关机），进程退出时
    // 内核自动清理 KVM 资源（fd close → VM teardown）。
    let _ = api_handle.join();
    eprintln!("terra-vmm: 收到 stop，进程退出");
    // 清理 socket 文件。
    let _ = std::fs::remove_file(&args.api_socket);
}

#[allow(clippy::too_many_arguments)]
fn handle_client(
    mut stream: UnixStream,
    stop_flag: &AtomicBool,
    mem_size_mib: usize,
    max_vcpu_count: u8,
    disk_attached: bool,
    resize_target: &Option<Arc<AtomicU64>>,
    mem_config: &Option<Arc<AtomicBool>>,
    blk_cap: &Option<Arc<AtomicU64>>,
    blk_cfg: &Option<Arc<AtomicBool>>,
) {
    let reader = BufReader::new(stream.try_clone().unwrap());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::error(format!("JSON 解析失败: {e}"));
                let _ = send_response(&mut stream, &resp);
                continue;
            }
        };

        let response = handle_request(
            request,
            stop_flag,
            mem_size_mib,
            max_vcpu_count,
            disk_attached,
            resize_target,
            mem_config,
            blk_cap,
            blk_cfg,
        );
        if send_response(&mut stream, &response).is_err() {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    req: Request,
    stop_flag: &AtomicBool,
    mem_size_mib: usize,
    max_vcpu_count: u8,
    disk_attached: bool,
    resize_target: &Option<Arc<AtomicU64>>,
    mem_config: &Option<Arc<AtomicBool>>,
    blk_cap: &Option<Arc<AtomicU64>>,
    blk_cfg: &Option<Arc<AtomicBool>>,
) -> Response {
    match req {
        Request::Stop => {
            stop_flag.store(true, Ordering::SeqCst);
            Response::ok()
        }
        Request::Status => {
            let info = StatusInfo {
                mem_size_mib,
                max_vcpu_count,
                disk_attached,
            };
            let data = serde_json::to_value(info).unwrap();
            Response::ok_with(data)
        }
        Request::ResizeMem { bytes } => match (resize_target, mem_config) {
            (Some(target), Some(config)) => {
                target.store(bytes, Ordering::SeqCst);
                config.store(true, Ordering::SeqCst);
                Response::ok()
            }
            _ => Response::error("virtio-mem device not configured"),
        },
        Request::ResizeDisk { bytes } => match (blk_cap, blk_cfg) {
            (Some(cap), Some(cfg)) => {
                cap.store(bytes / 512, Ordering::SeqCst);
                cfg.store(true, Ordering::SeqCst);
                Response::ok()
            }
            _ => Response::error("virtio-blk device not configured"),
        },
    }
}

fn send_response(stream: &mut UnixStream, resp: &Response) -> std::io::Result<()> {
    let mut json = serde_json::to_string(resp).unwrap();
    json.push('\n');
    stream.write_all(json.as_bytes())
}

//! terra-boot：最小启动示例，直接引导 bzImage 进入 guest shell。
//!
//! 用法：`cargo run -p vmm --example boot -- --kernel bzImage [--initrd initramfs]`

use std::path::PathBuf;

use clap::Parser;
use vmm_core::{Vm, VmConfig};

/// terra-boot 命令行参数。
#[derive(Parser)]
#[command(
    name = "terra-boot",
    about = "最小 VMM 启动示例：直接引导 bzImage 到 guest shell"
)]
struct Args {
    /// 内核 bzImage 路径
    #[arg(long)]
    kernel: PathBuf,
    /// initramfs 路径（可选）
    #[arg(long)]
    initrd: Option<PathBuf>,
    /// guest 内存大小（MiB）
    #[arg(long, default_value_t = 128)]
    mem_size_mib: usize,
    /// 内核命令行
    #[arg(long, default_value = "console=ttyS0 reboot=k panic=-1 tsc=reliable")]
    cmdline: String,
}

fn main() {
    let args = Args::parse();

    let config = VmConfig {
        mem_size_mib: args.mem_size_mib,
        kernel_path: args.kernel,
        initrd_path: args.initrd,
        kernel_cmdline: args.cmdline,
        ..VmConfig::default()
    };

    if let Err(e) = Vm::new(config).and_then(|mut vm| vm.run()) {
        eprintln!("terra-boot: {e}");
        std::process::exit(1);
    }
}

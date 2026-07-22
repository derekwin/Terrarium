//! terra CLI（M2 Task 5）。
//!
//! terra create / exec / ls / terminate —— 薄命令行，逻辑委托给 controller。

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use controller::Controller;

#[derive(Parser)]
#[command(name = "terra", about = "Terrarium CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 创建 VM
    Create {
        /// VM ID
        #[arg(long)]
        id: String,
        /// 内核路径
        #[arg(long, default_value = "target/guest/bzImage")]
        kernel: PathBuf,
        /// initramfs 路径
        #[arg(long)]
        initrd: Option<PathBuf>,
        /// 内存（MiB）
        #[arg(long, default_value_t = 128)]
        mem: usize,
        /// vCPU 数量
        #[arg(long, default_value_t = 1)]
        vcpus: u8,
    },
    /// 列出运行中的 VM
    Ls,
    /// 销毁 VM
    Terminate {
        /// VM ID
        id: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let ctrl = Controller::new();

    match cli.command {
        Commands::Create {
            id,
            kernel,
            initrd,
            mem,
            vcpus,
        } => match ctrl.create(&id, &kernel, initrd.as_deref(), mem, vcpus) {
            Ok(info) => println!("created: {info:?}"),
            Err(e) => {
                eprintln!("create: {e}");
                std::process::exit(1);
            }
        },
        Commands::Ls => {
            for info in ctrl.list() {
                println!("{}", info.id);
            }
        }
        Commands::Terminate { id } => {
            if let Err(e) = ctrl.destroy(&id) {
                eprintln!("terminate: {e}");
                std::process::exit(1);
            }
            println!("terminated: {id}");
        }
    }
}

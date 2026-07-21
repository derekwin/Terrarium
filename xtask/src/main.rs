//! xtask：项目构建辅助任务入口。
//!
//! M0 状态：骨架。Task 1 将在此实现 `cargo xtask kernel`：
//! 下载上游稳定版内核，应用最小裁剪配置编译 bzImage，并构建 initramfs
//! （静态 busybox），产物放 `target/guest/`（不进 git）。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "Terrarium 构建辅助任务")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 下载/配置/编译 guest 内核与 initramfs（Task 1 实现）
    Kernel {
        /// 上游稳定版内核版本，如 6.12.x
        #[arg(long)]
        version: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Kernel { .. } => {
            eprintln!("xtask kernel: 尚未实现（M0 Task 1）");
            std::process::exit(1);
        }
    }
}

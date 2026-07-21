//! terra-vmm：每个 VM 一个进程的 VMM 薄壳二进制。
//!
//! M0 状态：骨架。M0 的启动能力由 `examples/boot.rs`（cargo xtask / example）承载，
//! 本二进制在 M0 后期由 boot 示例演化而来（见 AGENTS.md 第 4 节）。

fn main() {
    eprintln!("terra-vmm: 尚未实现（M0 开发中），最小启动见 examples/boot.rs");
    std::process::exit(1);
}

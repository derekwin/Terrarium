//! Terrarium VMM 核心库。
//!
//! 职责：VM 生命周期、guest 地址空间、vCPU 管理。
//! 平台相关代码放在 `arch` 子模块（当前仅 x86_64 + KVM），
//! 公共接口不出现 x86 专有类型（见 AGENTS.md 第 3 节）。

mod arch;
pub mod device;
mod rtc;
mod serial;
mod vm;

pub use serial::Serial;
pub use vm::{Error, Vm, VmConfig};

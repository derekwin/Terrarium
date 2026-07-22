//! 平台相关代码的汇聚层。
//!
//! 按目标架构条件编译导出对应的子模块；公共接口（`Vm`、`VmConfig`、错误类型）
//! 不出现任何架构专有类型（见 AGENTS.md 第 3 节）。当前仅支持 x86_64 + KVM，
//! aarch64 暂缓。

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

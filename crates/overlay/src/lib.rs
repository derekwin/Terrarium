//! Three-layer OverlayFS via qcow2 backing chains.
//!
//! Architecture:
//!   base.qcow2           ← shared, read-only (OS + kernel modules)
//!   tool-python.qcow2    ← read-only, pre-built (Python runtime)
//!   user-a.qcow2         ← read-write, per-user (COW writes)
//!
//! qcow2 backing chain: user → tool → base

mod qcow2;
mod spec;

pub use qcow2::OverlayManager;
pub use spec::OverlaySpec;

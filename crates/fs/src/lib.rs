//! `terrarium-fs` — Independent filesystem crate for Terrarium Engine.
//!
//! Provides EROFS mount/resolution, layer build/list/remove, and cpio
//! pack/extract. Zero dependency on the CH adapter or engine crate.
//!
//! Also exposed as a Python extension module via PyO3 (maturin).

pub mod cpio;
pub mod erofs;
pub mod layer;

// Re-export the public API for convenience.
pub use cpio::{extract_cpio_layer, pack_cpio_rootfs};
pub use erofs::{is_mounted, mount_erofs};
pub use layer::{
    build_erofs_layer, list_layers, remove_layer, resolve_layer, validate_layer_name, LayerConfig,
};

// ---------------------------------------------------------------------------
// PyO3 module stub
// ---------------------------------------------------------------------------

#[cfg(feature = "pyo3")]
use pyo3::types::PyModuleMethods;

/// Python module for Terrarium filesystem operations.
///
/// Built with `maturin develop` / `pip install -e crates/fs`.
#[cfg(feature = "pyo3")]
#[pyo3::pymodule]
#[pyo3(name = "_fs")]
pub fn terrarium_fs_py(
    _py: pyo3::Python<'_>,
    m: &pyo3::Bound<'_, pyo3::types::PyModule>,
) -> pyo3::PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

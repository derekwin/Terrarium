//! `terrarium-fs` — Independent filesystem crate for Terrarium Engine.
//!
//! Provides EROFS mount/resolution, layer build/list/remove, and cpio
//! pack/extract. Zero dependency on the CH adapter or engine crate.
//!
//! Also exposed as a Python extension module via PyO3 (maturin).
// The #[pyfunction] proc macro expansion triggers this lint on the
// generated wrapper; function-level #[allow] does not propagate through
// proc macros, so a crate-level attribute is needed.
#![allow(clippy::useless_conversion)]

pub mod cpio;
pub mod erofs;
pub mod layer;

// Re-export the public API for convenience.
pub use cpio::{
    build_initramfs_agent, build_initramfs_virtiofs, extract_cpio_layer, pack_cpio_rootfs,
};
pub use erofs::{is_mounted, mount_erofs};
pub use layer::{
    build_erofs_layer, list_layers, remove_layer, resolve_layer, validate_layer_name, LayerConfig,
};

// ---------------------------------------------------------------------------
// PyO3 bindings — built with `maturin develop --features pyo3`
// ---------------------------------------------------------------------------

#[cfg(feature = "pyo3")]
use pyo3::prelude::*;

/// Build an EROFS layer image from a source directory.
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "build_erofs_layer")]
fn build_erofs_layer_py(src_dir: String, name: String, output_dir: String) -> PyResult<String> {
    crate::layer::build_erofs_layer(&src_dir, &name, &output_dir)
        .map_err(pyo3::exceptions::PyRuntimeError::new_err)
}

/// List available layer names under `layer_dir`.
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "list_layers")]
fn list_layers_py(layer_dir: String) -> Vec<String> {
    crate::layer::list_layers(&layer_dir)
}

/// Remove a layer by name from `layer_dir`.
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "remove_layer")]
fn remove_layer_py(name: String, layer_dir: String) -> PyResult<()> {
    crate::layer::remove_layer(&name, &layer_dir).map_err(pyo3::exceptions::PyRuntimeError::new_err)
}

/// Pack a layer directory into a bootable cpio.gz rootfs image.
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "pack_cpio_rootfs")]
fn pack_cpio_rootfs_py(layer_dir: String, name: String, output_dir: String) -> PyResult<String> {
    crate::cpio::pack_cpio_rootfs(&layer_dir, &name, &output_dir)
        .map_err(pyo3::exceptions::PyRuntimeError::new_err)
}

/// Extract a cpio.gz archive into a directory.
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "extract_cpio_layer")]
fn extract_cpio_layer_py(cpio_path: String, output_dir: String) -> PyResult<()> {
    crate::cpio::extract_cpio_layer(&cpio_path, &output_dir)
        .map_err(pyo3::exceptions::PyRuntimeError::new_err)
}

/// Build a warm-pool agent initramfs (FS-M4).
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "build_initramfs_agent")]
fn build_initramfs_agent_py(
    src_rootfs_dir: String,
    guest_proxy_binary: String,
    init_template: String,
    output: String,
) -> PyResult<String> {
    crate::cpio::build_initramfs_agent(
        &src_rootfs_dir,
        &guest_proxy_binary,
        &init_template,
        &output,
    )
    .map_err(pyo3::exceptions::PyRuntimeError::new_err)
}

/// Build a virtiofs bootstrap initramfs (FS-M1).
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "build_initramfs_virtiofs")]
fn build_initramfs_virtiofs_py(
    src_rootfs_dir: String,
    init_template: String,
    output: String,
) -> PyResult<String> {
    crate::cpio::build_initramfs_virtiofs(&src_rootfs_dir, &init_template, &output)
        .map_err(pyo3::exceptions::PyRuntimeError::new_err)
}

/// Resolve a layer name to a usable lowerdir path.
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "resolve_layer")]
fn resolve_layer_py(layer_dir: String, fs_root: String, name: String) -> PyResult<String> {
    let config = crate::layer::LayerConfig {
        layer_dir,
        fs_root,
        mounted_layers: std::sync::Arc::new(
            std::sync::Mutex::new(std::collections::HashSet::new()),
        ),
    };
    crate::layer::resolve_layer(&config, &name)
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
}

/// Validate a layer name against the allowed character set.
#[cfg(feature = "pyo3")]
#[pyfunction]
#[pyo3(name = "validate_layer_name")]
fn validate_layer_name_py(name: String) -> PyResult<()> {
    crate::layer::validate_layer_name(&name).map_err(pyo3::exceptions::PyValueError::new_err)
}

/// Python module for Terrarium filesystem operations.
///
/// Built with `maturin develop --features pyo3` / `pip install -e crates/fs`.
#[cfg(feature = "pyo3")]
#[pymodule]
fn terrarium_fs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(build_erofs_layer_py, m)?)?;
    m.add_function(wrap_pyfunction!(list_layers_py, m)?)?;
    m.add_function(wrap_pyfunction!(remove_layer_py, m)?)?;
    m.add_function(wrap_pyfunction!(pack_cpio_rootfs_py, m)?)?;
    m.add_function(wrap_pyfunction!(extract_cpio_layer_py, m)?)?;
    m.add_function(wrap_pyfunction!(build_initramfs_agent_py, m)?)?;
    m.add_function(wrap_pyfunction!(build_initramfs_virtiofs_py, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_layer_py, m)?)?;
    m.add_function(wrap_pyfunction!(validate_layer_name_py, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

// The #[pyfunction] proc macro expansion triggers this lint on the
// generated wrapper; function-level #[allow] does not propagate through
// proc macros, so a crate-level attribute is needed.
#![allow(clippy::useless_conversion)]

pub mod commands;
pub mod daemon;
pub mod manager;
pub mod policy;

use std::sync::Arc;

use adapter_cloud_hypervisor::ChAdapter;
use pyo3::prelude::*;

/// Start the Terrarium engine daemon in a background thread.
///
/// `embedded` must be true when the daemon runs inside a host process
/// (the normal SDK in-process usage). Embedded daemons refuse the
/// `daemon_stop` command, because stopping there would tear down the
/// host process. Pass `embedded=false` only when this process is a
/// dedicated daemon (e.g. a service spawned solely to run the engine).
#[pyfunction]
#[pyo3(signature = (socket_path, ch_binary=None, embedded=true))]
fn start_daemon(socket_path: String, ch_binary: Option<String>, embedded: bool) -> PyResult<()> {
    tracing_subscriber::fmt::init();

    let ch_bin = ch_binary.unwrap_or_else(|| "cloud-hypervisor".to_string());
    let adapter: Arc<dyn adapter_traits::VmAdapter> = Arc::new(ChAdapter::new(ch_bin));

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        rt.block_on(async {
            daemon::run(&socket_path, None, adapter, embedded)
                .await
                .expect("Daemon exited with error");
        });
    });

    Ok(())
}

/// Python module entry point for `import terrarium_engine`.
#[pymodule]
fn terrarium_engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(start_daemon, m)?)?;
    Ok(())
}

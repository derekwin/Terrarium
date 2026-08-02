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
use adapter_traits::SandboxAdapter;
use pyo3::prelude::*;

/// The default L2 sandbox backend for [`crate::manager::VmManager`].
///
/// With the default `sandlock` feature this is the guest-sandlock adapter;
/// without it, the engine builds anyway and every `create` fails with a
/// clear not-supported error (see [`UnavailableSandboxAdapter`]). The
/// concrete backend is hidden behind this factory so callers only ever see
/// `dyn SandboxAdapter`; inject a custom one via
/// [`VmManager::with_sandbox_adapter`].
pub fn default_sandbox_adapter() -> Box<dyn SandboxAdapter> {
    #[cfg(feature = "sandlock")]
    {
        Box::new(adapter_sandlock::GuestSandlockAdapter::new())
    }
    #[cfg(not(feature = "sandlock"))]
    {
        Box::new(UnavailableSandboxAdapter)
    }
}

/// Sandbox backend compiled in when the engine is built without the default
/// `sandlock` feature: `create` fails with a clear not-supported error, so
/// the crate still builds with a truthful failure mode instead of a missing
/// backend symbol.
#[cfg(not(feature = "sandlock"))]
pub struct UnavailableSandboxAdapter;

#[cfg(not(feature = "sandlock"))]
impl UnavailableSandboxAdapter {
    /// Message returned by every `create` on this backend.
    pub const ERROR_MESSAGE: &'static str = "no sandbox backend (enable the engine 'sandlock' feature or inject with VmManager::with_sandbox_adapter)";
}

#[cfg(not(feature = "sandlock"))]
#[async_trait::async_trait]
impl SandboxAdapter for UnavailableSandboxAdapter {
    async fn create(
        &self,
        _vm: Arc<dyn adapter_traits::VmHandle>,
        _spec: &adapter_traits::SandboxSpec,
    ) -> Result<Box<dyn adapter_traits::SandboxHandle>, adapter_traits::AdapterError> {
        Err(adapter_traits::AdapterError::not_supported(
            Self::ERROR_MESSAGE,
        ))
    }
}

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

#[cfg(test)]
mod default_sandbox_backend_tests {
    use super::*;

    #[test]
    fn factory_returns_a_usable_backend() {
        let adapter = default_sandbox_adapter();
        // The factory output must satisfy the `Send + Sync` backend contract
        // under every feature combination; a scoped-thread handoff proves it.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = adapter.as_ref();
            });
        });
    }

    #[cfg(not(feature = "sandlock"))]
    #[test]
    fn unavailable_backend_reports_a_clear_error() {
        struct DeadHandle;
        #[async_trait::async_trait]
        impl adapter_traits::VmHandle for DeadHandle {
            async fn info(&self) -> Result<adapter_traits::VmInfo, adapter_traits::AdapterError> {
                unreachable!("create never touches the vm handle")
            }
            async fn resize(
                &self,
                _cpu: Option<u32>,
                _memory: Option<u64>,
            ) -> Result<(), adapter_traits::AdapterError> {
                unreachable!("create never touches the vm handle")
            }
            async fn snapshot(
                &self,
            ) -> Result<adapter_traits::Snapshot, adapter_traits::AdapterError> {
                unreachable!("create never touches the vm handle")
            }
            async fn shutdown(&self) -> Result<(), adapter_traits::AdapterError> {
                unreachable!("create never touches the vm handle")
            }
            fn pid(&self) -> u32 {
                unreachable!("create never touches the vm handle")
            }
            fn is_alive(&self) -> bool {
                unreachable!("create never touches the vm handle")
            }
        }

        let backend = default_sandbox_adapter();
        let spec = adapter_traits::SandboxSpec {
            name: adapter_traits::VmName::new("sb-test").unwrap(),
            tools: Vec::new(),
            limits: Default::default(),
            env: Default::default(),
            policy: None,
        };
        let result = block_on_current_thread(backend.create(Arc::new(DeadHandle), &spec));
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("unavailable backend must fail create"),
        };
        assert!(matches!(
            &err,
            adapter_traits::AdapterError::NotSupported(_)
        ));
        // `AdapterError`'s Display prefixes the variant ("not supported: …").
        assert!(err
            .to_string()
            .contains(UnavailableSandboxAdapter::ERROR_MESSAGE));
    }

    #[cfg(not(feature = "sandlock"))]
    fn block_on_current_thread<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to build test runtime")
            .block_on(fut)
    }
}

//! Mock VmAdapter and VmHandle for unit testing VmManager.
//!
//! Provides controllable mock implementations that require no real KVM,
//! Cloud Hypervisor, or external processes.
//!
//! # Implementation note
//!
//! The `VmAdapter` and `VmHandle` traits are defined with `#[async_trait]`,
//! which expands each `async fn` into methods with explicit lifetime
//! parameters (`'life0`, `'life1`, …, `'async_trait`) and `where` clauses.
//! Because this integration-test crate cannot directly depend on the
//! `async-trait` proc-macro, every async method is manually desugared
//! to match the **exact** expanded signature (verified with `cargo expand`).
//!
//! # Usage
//!
//! ```rust,ignore
//! let adapter = MockVmAdapter::new()
//!     .with_state("Running")
//!     .with_pid(42)
//!     .with_exec("hello\n", "", 0);
//! let handle = adapter.create(&spec).await.unwrap();
//! assert_eq!(handle.pid(), 42);
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use adapter_traits::{
    AdapterError, ExecResult, FsSpec, NetworkQos, Snapshot, VmAdapter, VmCapabilities, VmHandle,
    VmInfo, VmSpec,
};

// ---------------------------------------------------------------------------
// MockVmHandle — internal state
// ---------------------------------------------------------------------------

struct MockState {
    pid: u32,
    alive: bool,
    state: String,
    exec_stdout: String,
    exec_stderr: String,
    exec_exit_code: i32,
    fs_attached: bool,
}

/// Controllable VM handle for unit tests.
pub struct MockVmHandle {
    inner: Mutex<MockState>,
}

impl MockVmHandle {
    fn new(
        pid: u32,
        alive: bool,
        state: String,
        exec_stdout: String,
        exec_stderr: String,
        exec_exit_code: i32,
        fs_attached: bool,
    ) -> Self {
        Self {
            inner: Mutex::new(MockState {
                pid,
                alive,
                state,
                exec_stdout,
                exec_stderr,
                exec_exit_code,
                fs_attached,
            }),
        }
    }
}

// --- VmHandle impl (exact expanded #[async_trait] signatures) ---

impl VmHandle for MockVmHandle {
    fn info<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<VmInfo, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let s = self.inner.lock().unwrap();
            Ok(VmInfo {
                state: s.state.clone(),
                cpus: Some(1),
                memory_mb: Some(256),
            })
        })
    }

    fn resize<'life0, 'async_trait>(
        &'life0 self,
        _cpu: Option<u32>,
        _memory: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(()) })
    }

    fn resize_disk<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _disk_id: &'life1 str,
        _size: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("resize_disk")) })
    }

    fn add_disk<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _path: &'life1 str,
        _disk_id: &'life2 str,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("add_disk")) })
    }

    fn set_network_qos<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _qos: &'life1 NetworkQos,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(()) })
    }

    fn exec<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _args: &'life1 [String],
        _timeout_secs: u64,
    ) -> Pin<Box<dyn Future<Output = Result<ExecResult, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let s = self.inner.lock().unwrap();
            Ok(ExecResult {
                stdout: s.exec_stdout.clone(),
                stderr: s.exec_stderr.clone(),
                exit_code: s.exec_exit_code,
            })
        })
    }

    fn attach_fs<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _fs: &'life1 FsSpec,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.inner.lock().unwrap().fs_attached = true;
            Ok(())
        })
    }

    fn detach_fs<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.inner.lock().unwrap().fs_attached = false;
            Ok(())
        })
    }

    fn snapshot<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<Snapshot, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("snapshot")) })
    }

    fn pause<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("pause")) })
    }

    fn resume<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("resume")) })
    }

    fn shutdown<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let mut s = self.inner.lock().unwrap();
            s.alive = false;
            s.state = "ShutDown".into();
            Ok(())
        })
    }

    fn pid(&self) -> u32 {
        self.inner.lock().unwrap().pid
    }

    fn is_alive(&mut self) -> bool {
        self.inner.lock().unwrap().alive
    }
}

// ---------------------------------------------------------------------------
// MockVmAdapter
// ---------------------------------------------------------------------------

/// Builder-pattern mock adapter for unit testing VmManager.
///
/// Builder methods set default values that are cloned into every
/// [`MockVmHandle`] returned by [`VmAdapter::create`].
///
/// # Examples
///
/// ```rust,ignore
/// let adapter = MockVmAdapter::new()
///     .with_state("Running")
///     .with_pid(42)
///     .with_exec("hello\n", "", 0);
/// ```
pub struct MockVmAdapter {
    capabilities: VmCapabilities,
    pid: u32,
    alive: bool,
    state: String,
    exec_stdout: String,
    exec_stderr: String,
    exec_exit_code: i32,
    fs_attached: bool,
}

impl MockVmAdapter {
    /// Create a new mock adapter with sensible defaults.
    ///
    /// Defaults: pid=0, alive=true, state="Created", empty exec result,
    /// fs_attached=false.
    pub fn new() -> Self {
        Self {
            capabilities: VmCapabilities::default(),
            pid: 0,
            alive: true,
            state: "Created".into(),
            exec_stdout: String::new(),
            exec_stderr: String::new(),
            exec_exit_code: 0,
            fs_attached: false,
        }
    }

    /// Set the PID reported by created handles.
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = pid;
        self
    }

    /// Set the initial VM state string (e.g. "Running", "Paused").
    pub fn with_state(mut self, state: &str) -> Self {
        self.state = state.to_string();
        self
    }

    /// Configure the exec result returned by created handles.
    pub fn with_exec(mut self, stdout: &str, stderr: &str, exit_code: i32) -> Self {
        self.exec_stdout = stdout.to_string();
        self.exec_stderr = stderr.to_string();
        self.exec_exit_code = exit_code;
        self
    }

    /// Set whether created handles report as alive.
    pub fn with_alive(mut self, alive: bool) -> Self {
        self.alive = alive;
        self
    }

    /// Set whether created handles start with a filesystem attached.
    pub fn with_fs_attached(mut self, attached: bool) -> Self {
        self.fs_attached = attached;
        self
    }

    /// Enable or disable a specific capability by name.
    ///
    /// Recognised names: `cpu_resize`, `memory_resize`, `disk_resize`,
    /// `disk_add`, `snapshot`, `pause_resume`, `network_qos`, `virtio_fs`.
    pub fn with_capability(mut self, name: &str, value: bool) -> Self {
        match name {
            "cpu_resize" => self.capabilities.cpu_resize = value,
            "memory_resize" => self.capabilities.memory_resize = value,
            "disk_resize" => self.capabilities.disk_resize = value,
            "disk_add" => self.capabilities.disk_add = value,
            "snapshot" => self.capabilities.snapshot = value,
            "pause_resume" => self.capabilities.pause_resume = value,
            "network_qos" => self.capabilities.network_qos = value,
            "virtio_fs" => self.capabilities.virtio_fs = value,
            _ => {}
        }
        self
    }

    /// Build a new handle from the current builder state (for advanced
    /// test scenarios that need a handle without going through `create`).
    pub fn build_handle(&self) -> MockVmHandle {
        MockVmHandle::new(
            self.pid,
            self.alive,
            self.state.clone(),
            self.exec_stdout.clone(),
            self.exec_stderr.clone(),
            self.exec_exit_code,
            self.fs_attached,
        )
    }
}

impl Default for MockVmAdapter {
    fn default() -> Self {
        Self::new()
    }
}

// --- VmAdapter impl (exact expanded #[async_trait] signatures) ---

impl VmAdapter for MockVmAdapter {
    fn capabilities(&self) -> VmCapabilities {
        self.capabilities.clone()
    }

    fn create<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _spec: &'life1 VmSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn VmHandle>, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(Box::new(self.build_handle()) as Box<dyn VmHandle>) })
    }

    fn restore<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _snapshot: &'life1 Snapshot,
        _spec: &'life2 VmSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn VmHandle>, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Err(AdapterError::not_supported("restore")) })
    }
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use adapter_traits::VmName;

    fn test_spec() -> VmSpec {
        VmSpec {
            name: VmName::new("test-vm").unwrap(),
            kernel: "/fake/vmlinux".into(),
            cmdline: None,
            boot_vcpus: 1,
            max_vcpus: Some(4),
            memory_mb: 256,
            max_memory_mb: Some(1024),
            initramfs: None,
            net: false,
            fs: None,
            backend_config: None,
        }
    }

    #[tokio::test]
    async fn test_create_and_info() {
        let adapter = MockVmAdapter::new().with_state("Running").with_pid(42);
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        assert_eq!(handle.pid(), 42);

        let info = handle.info().await.unwrap();
        assert_eq!(info.state, "Running");
    }

    #[tokio::test]
    async fn test_is_alive_and_shutdown() {
        let adapter = MockVmAdapter::new().with_alive(true);
        let spec = test_spec();
        let mut handle = adapter.create(&spec).await.unwrap();

        assert!(handle.is_alive());

        handle.shutdown().await.unwrap();
        assert!(!handle.is_alive());

        let info = handle.info().await.unwrap();
        assert_eq!(info.state, "ShutDown");
    }

    #[tokio::test]
    async fn test_exec_result() {
        let adapter = MockVmAdapter::new().with_exec("hello\n", "oops\n", 1);
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        let result = handle
            .exec(&["echo".into(), "hello".into()], 10)
            .await
            .unwrap();
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.stderr, "oops\n");
        assert_eq!(result.exit_code, 1);
    }

    #[tokio::test]
    async fn test_snapshot_not_supported() {
        let adapter = MockVmAdapter::new();
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        assert!(handle.snapshot().await.is_err());
    }

    #[tokio::test]
    async fn test_pause_resume_not_supported() {
        let adapter = MockVmAdapter::new();
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        assert!(handle.pause().await.is_err());
        assert!(handle.resume().await.is_err());
    }

    #[tokio::test]
    async fn test_restore_not_supported() {
        let adapter = MockVmAdapter::new();
        let snapshot = Snapshot {
            path: "/fake".into(),
        };
        let spec = test_spec();
        assert!(adapter.restore(&snapshot, &spec).await.is_err());
    }

    #[tokio::test]
    async fn test_attach_detach_fs() {
        let adapter = MockVmAdapter::new().with_fs_attached(false);
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        let fs = FsSpec {
            layers: vec!["base".into()],
            upper: adapter_traits::UpperPolicy::Ephemeral,
        };

        handle.attach_fs(&fs).await.unwrap();
        handle.detach_fs().await.unwrap();
    }

    #[tokio::test]
    async fn test_capabilities() {
        let adapter = MockVmAdapter::new()
            .with_capability("cpu_resize", true)
            .with_capability("memory_resize", true);
        let caps = adapter.capabilities();
        assert!(caps.cpu_resize);
        assert!(caps.memory_resize);
        // Defaults should be false
        assert!(!caps.snapshot);
        assert!(!caps.pause_resume);
    }

    #[tokio::test]
    async fn test_resize_noop() {
        let adapter = MockVmAdapter::new();
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        // Resize is a no-op success in the mock
        handle.resize(Some(2), Some(512)).await.unwrap();
        // resize_disk is NotSupported
        assert!(handle.resize_disk("vda", 1024).await.is_err());
        // add_disk is NotSupported
        assert!(handle.add_disk("/dev/vdb", "vdb").await.is_err());
    }

    #[tokio::test]
    async fn test_network_qos_noop() {
        let adapter = MockVmAdapter::new();
        let spec = test_spec();
        let handle = adapter.create(&spec).await.unwrap();

        let qos = NetworkQos {
            egress_kbps: 1000,
            ingress_kbps: 500,
            priority: 1,
        };
        handle.set_network_qos(&qos).await.unwrap();
    }

    #[tokio::test]
    async fn test_builder_chaining() {
        let adapter = MockVmAdapter::new()
            .with_state("Running")
            .with_pid(42)
            .with_exec("hello\n", "", 0)
            .with_alive(true)
            .with_fs_attached(true)
            .with_capability("cpu_resize", true);

        let spec = test_spec();
        let mut handle = adapter.create(&spec).await.unwrap();

        assert_eq!(handle.pid(), 42);
        assert!(handle.is_alive());

        let result = handle.exec(&["true".into()], 5).await.unwrap();
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);
    }
}

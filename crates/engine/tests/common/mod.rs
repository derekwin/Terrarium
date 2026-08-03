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
use std::sync::{Arc, Mutex};

use adapter_traits::{
    AdapterError, ExecCommand, ExecOpts, ExecResult, FsSpec, SandboxAdapter, SandboxHandle,
    SandboxPolicy, SandboxSpec, Snapshot, VmAdapter, VmHandle, VmInfo, VmSpec,
};

/// One recorded exec invocation (for assertions on engine→guest plumbing).
/// Fields are read only by tests that assert on the plumbing.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ExecCall {
    pub args: Vec<String>,
    pub timeout_secs: u64,
    pub sandbox: bool,
    pub work_dir: Option<String>,
    pub exec_id: Option<String>,
    pub policy: Option<SandboxPolicy>,
}

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
    exec_failures: u32,
    fs_attached: bool,
    cpus: u8,
}

/// Controllable VM handle for unit tests.
pub struct MockVmHandle {
    inner: Mutex<MockState>,
    exec_log: Arc<Mutex<Vec<ExecCall>>>,
    kill_log: Arc<Mutex<Vec<String>>>,
    /// When set, exec calls carrying an exec_id (background sessions)
    /// park on this gate before returning — lets tests observe a
    /// genuinely "running" background session. Plain blocking execs
    /// (mkdir, rm, ...) never park.
    exec_gate: Option<Arc<tokio::sync::Notify>>,
    /// When set, exec calls WITHOUT an exec_id (blocking execs) park on
    /// this gate before returning — lets tests observe a blocking exec
    /// that is genuinely in flight (e.g. the daemon lock-free path).
    blocking_exec_gate: Option<Arc<tokio::sync::Notify>>,
    /// Shared ping counter (readiness probe assertions).
    ping_count: Arc<Mutex<u32>>,
    /// First N pings fail ("agent not up yet"), then pings succeed.
    ping_ready_after: u32,
}

impl MockVmHandle {
    #[allow(clippy::too_many_arguments)]
    fn new(
        pid: u32,
        alive: bool,
        state: String,
        exec_stdout: String,
        exec_stderr: String,
        exec_exit_code: i32,
        exec_failures: u32,
        fs_attached: bool,
        exec_log: Arc<Mutex<Vec<ExecCall>>>,
        kill_log: Arc<Mutex<Vec<String>>>,
        exec_gate: Option<Arc<tokio::sync::Notify>>,
        blocking_exec_gate: Option<Arc<tokio::sync::Notify>>,
        ping_count: Arc<Mutex<u32>>,
        ping_ready_after: u32,
        cpus: u8,
    ) -> Self {
        Self {
            inner: Mutex::new(MockState {
                pid,
                alive,
                state,
                exec_stdout,
                exec_stderr,
                exec_exit_code,
                exec_failures,
                fs_attached,
                cpus,
            }),
            exec_log,
            kill_log,
            exec_gate,
            blocking_exec_gate,
            ping_count,
            ping_ready_after,
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
                cpus: Some(s.cpus),
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

    fn exec<'life0, 'life1, 'async_trait>(
        &'life0 self,
        opts: &'life1 ExecOpts,
    ) -> Pin<Box<dyn Future<Output = Result<ExecResult, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.exec_log.lock().unwrap().push(ExecCall {
                args: opts.args.clone(),
                timeout_secs: opts.timeout_secs,
                sandbox: opts.sandbox,
                work_dir: opts.work_dir.clone(),
                exec_id: opts.exec_id.clone(),
                policy: opts.policy.clone(),
            });
            if let (Some(gate), Some(_)) = (&self.exec_gate, &opts.exec_id) {
                gate.notified().await;
            }
            if let (Some(gate), None) = (&self.blocking_exec_gate, &opts.exec_id) {
                gate.notified().await;
            }
            let mut s = self.inner.lock().unwrap();
            if s.exec_failures > 0 {
                s.exec_failures -= 1;
                drop(s);
                return Err(AdapterError::internal(
                    "vsock handshake rejected: guest agent still booting",
                ));
            }
            Ok(ExecResult {
                stdout: s.exec_stdout.clone(),
                stderr: s.exec_stderr.clone(),
                exit_code: s.exec_exit_code,
            })
        })
    }

    fn kill_exec<'life0, 'life1, 'async_trait>(
        &'life0 self,
        exec_id: &'life1 str,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.kill_log.lock().unwrap().push(exec_id.to_string());
            Ok(())
        })
    }

    fn ping<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let mut n = self.ping_count.lock().unwrap();
            *n += 1;
            if *n <= self.ping_ready_after {
                Err(AdapterError::internal("guest agent not ready"))
            } else {
                Ok(())
            }
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

    fn snapshot<'life0, 'life1, 'async_trait>(
        &'life0 self,
        path: &'life1 str,
    ) -> Pin<Box<dyn Future<Output = Result<Snapshot, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(Snapshot {
                path: path.to_string(),
            })
        })
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

    fn is_alive(&self) -> bool {
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
    pid: u32,
    alive: bool,
    state: String,
    exec_stdout: String,
    exec_stderr: String,
    exec_exit_code: i32,
    exec_failures: u32,
    fs_attached: bool,
    exec_log: Arc<Mutex<Vec<ExecCall>>>,
    kill_log: Arc<Mutex<Vec<String>>>,
    exec_gate: Option<Arc<tokio::sync::Notify>>,
    blocking_exec_gate: Option<Arc<tokio::sync::Notify>>,
    ping_count: Arc<Mutex<u32>>,
    ping_ready_after: u32,
    cpus: u8,
}

impl MockVmAdapter {
    /// Create a new mock adapter with sensible defaults.
    ///
    /// Defaults: pid=0, alive=true, state="Created", empty exec result,
    /// fs_attached=false.
    pub fn new() -> Self {
        Self {
            pid: 0,
            alive: true,
            state: "Created".into(),
            exec_stdout: String::new(),
            exec_stderr: String::new(),
            exec_exit_code: 0,
            exec_failures: 0,
            fs_attached: false,
            exec_log: Arc::new(Mutex::new(Vec::new())),
            kill_log: Arc::new(Mutex::new(Vec::new())),
            exec_gate: None,
            blocking_exec_gate: None,
            ping_count: Arc::new(Mutex::new(0)),
            ping_ready_after: 0,
            cpus: 1,
        }
    }

    /// Make the first N pings fail (guest agent still booting), then
    /// succeed — exercises the pool readiness wait.
    #[allow(dead_code)]
    pub fn with_ping_ready_after(mut self, n: u32) -> Self {
        self.ping_ready_after = n;
        self
    }

    /// Shared count of ping calls made on handles from this adapter.
    #[allow(dead_code)]
    pub fn ping_count(&self) -> Arc<Mutex<u32>> {
        self.ping_count.clone()
    }

    /// Park execs that carry an exec_id (background sessions) on this
    /// gate until `notify_one()` — keeps a background session "running".
    #[allow(dead_code)]
    pub fn with_exec_gate(mut self, gate: Arc<tokio::sync::Notify>) -> Self {
        self.exec_gate = Some(gate);
        self
    }

    /// Park execs WITHOUT an exec_id (blocking execs) on this gate until
    /// `notify_one()` — keeps a blocking exec genuinely in flight.
    #[allow(dead_code)]
    pub fn with_blocking_exec_gate(mut self, gate: Arc<tokio::sync::Notify>) -> Self {
        self.blocking_exec_gate = Some(gate);
        self
    }

    /// Shared log of every exec call made on handles from this adapter.
    #[allow(dead_code)]
    pub fn exec_log(&self) -> Arc<Mutex<Vec<ExecCall>>> {
        self.exec_log.clone()
    }

    /// Shared log of every kill_exec id sent to handles from this adapter.
    #[allow(dead_code)]
    pub fn kill_log(&self) -> Arc<Mutex<Vec<String>>> {
        self.kill_log.clone()
    }

    /// Set the PID reported by created handles.
    #[allow(dead_code)]
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = pid;
        self
    }

    /// Set the initial VM state string (e.g. "Running", "Paused").
    #[allow(dead_code)]
    pub fn with_state(mut self, state: &str) -> Self {
        self.state = state.to_string();
        self
    }

    /// Configure the exec result returned by created handles.
    #[allow(dead_code)]
    pub fn with_exec(mut self, stdout: &str, stderr: &str, exit_code: i32) -> Self {
        self.exec_stdout = stdout.to_string();
        self.exec_stderr = stderr.to_string();
        self.exec_exit_code = exit_code;
        self
    }

    /// Set whether created handles report as alive.
    #[allow(dead_code)]
    pub fn with_alive(mut self, alive: bool) -> Self {
        self.alive = alive;
        self
    }

    /// Set whether created handles start with a filesystem attached.
    #[allow(dead_code)]
    pub fn with_fs_attached(mut self, attached: bool) -> Self {
        self.fs_attached = attached;
        self
    }

    /// Set the CPU count reported by created handles' info().
    #[allow(dead_code)]
    pub fn with_cpus(mut self, cpus: u8) -> Self {
        self.cpus = cpus;
        self
    }

    /// Make the first N execs fail with a vsock handshake error (guest
    /// agent still booting), then succeed.
    #[allow(dead_code)]
    pub fn with_exec_failures(mut self, n: u32) -> Self {
        self.exec_failures = n;
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
            self.exec_failures,
            self.fs_attached,
            self.exec_log.clone(),
            self.kill_log.clone(),
            self.exec_gate.clone(),
            self.blocking_exec_gate.clone(),
            self.ping_count.clone(),
            self.ping_ready_after,
            self.cpus,
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
        Box::pin(async move { Ok(Box::new(self.build_handle()) as Box<dyn VmHandle>) })
    }
}

// ---------------------------------------------------------------------------
// MockSandboxAdapter / MockSandboxHandle — C3 wiring tests
// ---------------------------------------------------------------------------

/// One recorded `SandboxAdapter::create` call (the spec is what the engine
/// binds at sandbox_create — asserting on its effective policy).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SandboxCreateCall {
    pub spec: SandboxSpec,
}

/// One recorded `SandboxHandle::exec` call (the engine-side observable of
/// a sandboxed blocking exec routed through the session handle).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SandboxHandleCall {
    pub args: Vec<String>,
    pub work_dir: Option<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub policy_override: Option<SandboxPolicy>,
    pub timeout_secs: Option<u64>,
}

struct MockSandboxState {
    exec_stdout: String,
    exec_stderr: String,
    exec_exit_code: i32,
}

/// Controllable sandbox handle for unit tests: records every exec command
/// and returns the configured result without touching any VM.
pub struct MockSandboxHandle {
    inner: Mutex<MockSandboxState>,
    exec_log: Arc<Mutex<Vec<SandboxHandleCall>>>,
    destroy_count: Arc<Mutex<u32>>,
}

impl MockSandboxHandle {
    fn new(
        exec_stdout: String,
        exec_stderr: String,
        exec_exit_code: i32,
        exec_log: Arc<Mutex<Vec<SandboxHandleCall>>>,
        destroy_count: Arc<Mutex<u32>>,
    ) -> Self {
        Self {
            inner: Mutex::new(MockSandboxState {
                exec_stdout,
                exec_stderr,
                exec_exit_code,
            }),
            exec_log,
            destroy_count,
        }
    }
}

impl SandboxHandle for MockSandboxHandle {
    fn exec<'life0, 'life1, 'async_trait>(
        &'life0 self,
        cmd: &'life1 ExecCommand,
    ) -> Pin<Box<dyn Future<Output = Result<ExecResult, AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.exec_log.lock().unwrap().push(SandboxHandleCall {
                args: cmd.args.clone(),
                work_dir: cmd.work_dir.clone(),
                env: cmd.env.clone(),
                policy_override: cmd.policy_override.clone(),
                timeout_secs: cmd.timeout_secs,
            });
            let s = self.inner.lock().unwrap();
            Ok(ExecResult {
                stdout: s.exec_stdout.clone(),
                stderr: s.exec_stderr.clone(),
                exit_code: s.exec_exit_code,
            })
        })
    }

    fn destroy<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AdapterError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            *self.destroy_count.lock().unwrap() += 1;
            Ok(())
        })
    }
}

/// Builder-pattern mock sandbox adapter: every `create` returns a fresh
/// [`MockSandboxHandle`] sharing the adapter's exec log and destroy count.
pub struct MockSandboxAdapter {
    exec_stdout: String,
    exec_stderr: String,
    exec_exit_code: i32,
    create_log: Arc<Mutex<Vec<SandboxCreateCall>>>,
    exec_log: Arc<Mutex<Vec<SandboxHandleCall>>>,
    destroy_count: Arc<Mutex<u32>>,
}

impl MockSandboxAdapter {
    /// Defaults: empty exec result, exit code 0.
    pub fn new() -> Self {
        Self {
            exec_stdout: String::new(),
            exec_stderr: String::new(),
            exec_exit_code: 0,
            create_log: Arc::new(Mutex::new(Vec::new())),
            exec_log: Arc::new(Mutex::new(Vec::new())),
            destroy_count: Arc::new(Mutex::new(0)),
        }
    }

    /// Configure the exec result returned by created handles.
    #[allow(dead_code)]
    pub fn with_exec(mut self, stdout: &str, stderr: &str, exit_code: i32) -> Self {
        self.exec_stdout = stdout.to_string();
        self.exec_stderr = stderr.to_string();
        self.exec_exit_code = exit_code;
        self
    }

    /// Shared log of every `create` call (the bound `SandboxSpec`).
    #[allow(dead_code)]
    pub fn create_log(&self) -> Arc<Mutex<Vec<SandboxCreateCall>>> {
        self.create_log.clone()
    }

    /// Shared log of every `handle.exec` call made on created handles.
    #[allow(dead_code)]
    pub fn exec_log(&self) -> Arc<Mutex<Vec<SandboxHandleCall>>> {
        self.exec_log.clone()
    }

    /// Shared count of `handle.destroy` calls made on created handles.
    #[allow(dead_code)]
    pub fn destroy_count(&self) -> Arc<Mutex<u32>> {
        self.destroy_count.clone()
    }
}

impl Default for MockSandboxAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxAdapter for MockSandboxAdapter {
    fn create<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _vm: Arc<dyn VmHandle>,
        spec: &'life1 SandboxSpec,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Box<dyn SandboxHandle>, AdapterError>> + Send + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.create_log
                .lock()
                .unwrap()
                .push(SandboxCreateCall { spec: spec.clone() });
            Ok(Box::new(MockSandboxHandle::new(
                self.exec_stdout.clone(),
                self.exec_stderr.clone(),
                self.exec_exit_code,
                self.exec_log.clone(),
                self.destroy_count.clone(),
            )) as Box<dyn SandboxHandle>)
        })
    }
}

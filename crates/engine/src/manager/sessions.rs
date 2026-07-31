//! Background exec sessions: async execs tracked at the protocol level.
//!
//! Holds the [`SessionInfo`] type and all session operations. The session
//! map itself stays a flat field on [`VmManager`]; these methods are split
//! out here because they only touch the sessions map plus the VM handle
//! lookup (`get_handle` on `self`).

use adapter_traits::{AdapterError, ExecOpts, ExecPolicy};

use super::VmManager;

/// Information about a background exec session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub vm_name: String,
    pub args: Vec<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// Sandbox this session belongs to (sandbox_exec), if any.
    pub sandbox: Option<String>,
}

impl VmManager {
    /// Start a background exec session. Returns immediately with a session_id.
    /// The actual execution runs in a spawned task that updates session status on completion.
    /// The session id is also registered in the guest as the exec_id, so
    /// `session_kill` can killpg it. `sandbox_id` links the session to an
    /// engine-level sandbox (sandbox_exec).
    #[allow(clippy::too_many_arguments)]
    pub async fn exec_background(
        &mut self,
        name: &str,
        args: &[String],
        timeout_secs: u64,
        sandbox: bool,
        session_id: &str,
        work_dir: Option<&str>,
        sandbox_id: Option<String>,
        policy: Option<ExecPolicy>,
    ) -> Result<(), AdapterError> {
        let handle = self
            .get_handle(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;

        let args = args.to_vec();
        let sid = session_id.to_string();
        let vm_name = name.to_string();
        let work_dir = work_dir.map(String::from);
        let sessions = self.sessions.clone();

        sessions.lock().unwrap().insert(
            sid.clone(),
            SessionInfo {
                session_id: sid.clone(),
                vm_name: vm_name.clone(),
                args: args.clone(),
                status: "running".to_string(),
                exit_code: None,
                stdout: None,
                stderr: None,
                sandbox: sandbox_id,
            },
        );

        tokio::spawn(async move {
            let mut opts = ExecOpts::new(args, timeout_secs)
                .with_sandbox(sandbox)
                .with_exec_id(&sid);
            if let Some(work_dir) = work_dir {
                opts = opts.with_work_dir(work_dir);
            }
            opts.policy = policy;
            let result = handle.exec(&opts).await;
            let mut sessions = sessions.lock().unwrap();
            if let Some(info) = sessions.get_mut(&sid) {
                // A killed session stays killed — never overwrite with the
                // completion that the SIGKILL itself triggered.
                if info.status != "running" {
                    return;
                }
                match result {
                    Ok(r) => {
                        info.status = "completed".to_string();
                        info.exit_code = Some(r.exit_code);
                        info.stdout = Some(r.stdout);
                        info.stderr = Some(r.stderr);
                    }
                    Err(e) => {
                        info.status = "failed".to_string();
                        info.stderr = Some(e.to_string());
                    }
                }
            }
        });

        Ok(())
    }

    /// Kill a running background exec session: killpg it in the guest via
    /// a fresh vsock connection, then mark it killed. The completion path
    /// will not overwrite the "killed" status.
    pub async fn session_kill(&self, session_id: &str) -> Result<(), AdapterError> {
        let (vm_name, status) = {
            let sessions = self.sessions.lock().unwrap();
            let info = sessions.get(session_id).ok_or_else(|| {
                AdapterError::not_found(format!("Session '{}' not found", session_id))
            })?;
            (info.vm_name.clone(), info.status.clone())
        };
        if status != "running" {
            return Err(AdapterError::invalid_argument(format!(
                "Session '{}' is not running (status: {})",
                session_id, status
            )));
        }
        let handle = self
            .get_handle(&vm_name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", vm_name)))?;
        handle.kill_exec(session_id).await?;
        if let Some(info) = self.sessions.lock().unwrap().get_mut(session_id) {
            info.status = "killed".to_string();
        }
        Ok(())
    }

    /// Get the status of a background exec session.
    pub fn session_status(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    /// List all session IDs with their status.
    pub fn session_list(&self) -> Vec<SessionInfo> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    /// Mark every running session on `vm_name` terminated. Called by
    /// `VmManager::unregister`: the VM is gone, so those sessions can
    /// never complete — the completion task never overwrites a
    /// non-running status.
    pub(super) fn terminate_sessions(&self, vm_name: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        for info in sessions.values_mut() {
            if info.vm_name == vm_name && info.status == "running" {
                info.status = "terminated".to_string();
            }
        }
    }
}

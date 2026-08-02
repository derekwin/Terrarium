//! Background exec sessions: async execs tracked at the protocol level.
//!
//! Holds the [`SessionInfo`] type and all session operations. The session
//! map itself stays a flat field on [`VmManager`]; these methods are split
//! out here because they only touch the sessions map plus the VM handle
//! lookup (`get_handle` on `self`).

use adapter_traits::{AdapterError, ExecOpts, SandboxPolicy};

use super::VmManager;

/// Cap on retained background sessions. Completed/killed/terminated
/// records are pruned on insert so a long-running daemon cannot grow the
/// session map without bound; `session_status` keeps querying the most
/// recent MAX_RETAINED_SESSIONS.
const MAX_RETAINED_SESSIONS: usize = 100;

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
        policy: Option<SandboxPolicy>,
    ) -> Result<(), AdapterError> {
        let handle = self
            .get_handle(name)
            .ok_or_else(|| AdapterError::not_found(format!("VM '{}' not found", name)))?;

        let args = args.to_vec();
        let sid = session_id.to_string();
        let vm_name = name.to_string();
        // Audit subject id: the linked engine sandbox id (sandbox_exec),
        // else the vm name.
        let audit_id = sandbox_id.clone().unwrap_or_else(|| vm_name.clone());
        let work_dir = work_dir.map(String::from);
        let sessions = self.sessions.clone();

        {
            let mut sessions = sessions.lock().unwrap();
            sessions.insert(
                sid.clone(),
                SessionInfo {
                    session_id: sid.clone(),
                    vm_name,
                    args: args.clone(),
                    status: "running".to_string(),
                    exit_code: None,
                    stdout: None,
                    stderr: None,
                    sandbox: sandbox_id.clone(),
                },
            );
            prune_sessions(&mut sessions);
        }

        tokio::spawn(async move {
            let mut opts = ExecOpts::new(args, timeout_secs)
                .with_sandbox(sandbox)
                .with_exec_id(&sid);
            if let Some(work_dir) = work_dir {
                opts = opts.with_work_dir(work_dir);
            }
            opts.policy = policy;
            let start = std::time::Instant::now();
            let result = handle.exec(&opts).await;
            // Audit the completion regardless of session state: a killed or
            // terminated session still ran its exec, and the outcome must
            // be observable. `opts.policy` is the effective policy the exec
            // ran with (the gating AuditSpec).
            crate::audit::audit_exec_outcome(
                opts.policy.as_ref(),
                &audit_id,
                &opts.args,
                &result,
                start.elapsed().as_millis() as u64,
            );
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

/// Prune the oldest non-running session when the map exceeds the cap,
/// so the daemon cannot grow without bound across many background execs.
/// Running sessions are never pruned (their results are still pending);
/// among terminal records the exact oldest is dropped (HashMap order is
/// arbitrary, but any terminal record is equally stale for the cap).
fn prune_sessions(sessions: &mut std::collections::HashMap<String, SessionInfo>) {
    while sessions.len() > MAX_RETAINED_SESSIONS {
        let victim = sessions
            .iter()
            .find(|(_, v)| v.status != "running")
            .map(|(k, _)| k.clone());
        match victim {
            Some(k) => {
                sessions.remove(&k);
            }
            None => break, // all running: never prune an active session
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(sid: &str, status: &str) -> SessionInfo {
        SessionInfo {
            session_id: sid.into(),
            vm_name: "tenant-x".into(),
            args: vec!["echo".into()],
            status: status.into(),
            exit_code: None,
            stdout: None,
            stderr: None,
            sandbox: None,
        }
    }

    #[test]
    fn prune_removes_terminal_sessions_over_cap() {
        let mut m = std::collections::HashMap::new();
        // 1 running + MAX_RETAINED_SESSIONS + 5 completed = over cap
        m.insert("run".into(), info("run", "running"));
        for i in 0..(MAX_RETAINED_SESSIONS + 5) {
            m.insert(format!("done-{i}"), info(&format!("done-{i}"), "completed"));
        }
        prune_sessions(&mut m);
        assert!(m.len() <= MAX_RETAINED_SESSIONS);
        assert!(m.contains_key("run"), "running session must survive");
    }

    #[test]
    fn prune_never_drops_running_sessions() {
        let mut m = std::collections::HashMap::new();
        for i in 0..(MAX_RETAINED_SESSIONS + 10) {
            m.insert(format!("run-{i}"), info(&format!("run-{i}"), "running"));
        }
        prune_sessions(&mut m);
        assert_eq!(
            m.len(),
            MAX_RETAINED_SESSIONS + 10,
            "all running, none pruned"
        );
    }

    #[test]
    fn prune_under_cap_is_noop() {
        let mut m = std::collections::HashMap::new();
        for i in 0..3 {
            m.insert(format!("s-{i}"), info(&format!("s-{i}"), "completed"));
        }
        prune_sessions(&mut m);
        assert_eq!(m.len(), 3);
    }
}

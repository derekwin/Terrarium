//! Registry of in-flight exec sessions, keyed by client-supplied `exec_id`.
//!
//! The host (engine) registers a background exec under an id so a later
//! `{"command":"kill","exec_id":...}` — arriving on a separate connection —
//! can killpg the whole process group.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// exec_id charset: `[a-zA-Z0-9-]+` (engine session ids are UUIDs).
pub fn validate_exec_id(id: &str) -> Result<(), String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!(
            "invalid exec_id {:?}: only [a-zA-Z0-9-] allowed",
            id
        ));
    }
    Ok(())
}

/// In-flight exec sessions, keyed by client-supplied `exec_id`.
///
/// A standalone struct so tests can use isolated instances (the
/// production daemon uses the process-wide [`registry()`] singleton).
pub struct Registry {
    inner: Mutex<HashMap<String, i32>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Register `pid` under `id`. Fails on a duplicate id — the caller
    /// must not have two live execs sharing one id.
    pub fn register(&self, id: &str, pid: i32) -> Result<(), String> {
        let mut reg = self.inner.lock().unwrap();
        if reg.contains_key(id) {
            return Err(format!("duplicate exec_id {:?}", id));
        }
        reg.insert(id.to_string(), pid);
        Ok(())
    }

    /// Remove `id` from the registry (exec finished).
    pub fn unregister(&self, id: &str) {
        self.inner.lock().unwrap().remove(id);
    }

    /// SIGKILL the process group registered under `id`.
    /// Unknown (or already finished) id → "session not found".
    pub fn kill(&self, id: &str) -> Result<(), String> {
        let pid = self
            .inner
            .lock()
            .unwrap()
            .get(id)
            .copied()
            .ok_or_else(|| "session not found".to_string())?;
        // SAFETY: pid was captured from Command::spawn() of a child that
        // was spawned with process_group(0), so -pid addresses its group.
        // killpg(-pid, SIGKILL) kills the entire group; if the child
        // already exited the kill is a harmless no-op (ESRCH).
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        Ok(())
    }

    /// SIGKILL every registered exec process group (in-place episode
    /// reset) and clear the registry. Returns how many groups were
    /// killed.
    pub fn kill_all(&self) -> usize {
        let mut reg = self.inner.lock().unwrap();
        let count = reg.len();
        for (_, pid) in reg.drain() {
            // SAFETY: pids were captured from Command::spawn() children
            // spawned with process_group(0), so -pid addresses their group.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        count
    }
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::new)
}

/// Register `pid` under `id` on the process-wide registry.
pub fn register(id: &str, pid: i32) -> Result<(), String> {
    registry().register(id, pid)
}

/// Remove `id` from the process-wide registry (exec finished).
pub fn unregister(id: &str) {
    registry().unregister(id);
}

/// RAII guard: unregisters the id on drop, on every exit path.
pub struct UnregisterGuard(String);

impl UnregisterGuard {
    pub fn new(id: &str) -> Self {
        Self(id.to_string())
    }
}

impl Drop for UnregisterGuard {
    fn drop(&mut self) {
        unregister(&self.0);
    }
}

/// SIGKILL the process group registered under `id` on the process-wide
/// registry.
pub fn kill(id: &str) -> Result<(), String> {
    registry().kill(id)
}

/// SIGKILL every registered exec process group on the process-wide
/// registry and clear it.
pub fn kill_all() -> usize {
    registry().kill_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ids_accepted() {
        assert!(validate_exec_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_exec_id("sb-1234abcd").is_ok());
        assert!(validate_exec_id("ABCdef123").is_ok());
    }

    #[test]
    fn invalid_ids_rejected() {
        assert!(validate_exec_id("").is_err());
        assert!(validate_exec_id("bad/id").is_err());
        assert!(validate_exec_id("bad id").is_err());
        assert!(validate_exec_id("bad_id").is_err());
        assert!(validate_exec_id("bad;id").is_err());
    }

    #[test]
    fn register_lookup_unregister() {
        let reg = Registry::new();
        let id = "test-reg-lookup";
        reg.register(id, 12345).unwrap();
        assert_eq!(reg.kill(id), Ok(()), "registered entry is killable");
        reg.unregister(id);
        assert_eq!(reg.kill(id), Err("session not found".to_string()));
    }

    #[test]
    fn duplicate_register_fails() {
        let reg = Registry::new();
        let id = "test-reg-dup";
        reg.register(id, 111).unwrap();
        let err = reg.register(id, 222).unwrap_err();
        assert!(err.contains("duplicate exec_id"));
        assert_eq!(reg.kill(id), Ok(()), "original registration untouched");
    }

    #[test]
    fn unregister_guard_drops_registration() {
        let id = format!("test-reg-guard-{}", std::process::id());
        register(&id, 333).unwrap();
        {
            let _guard = UnregisterGuard::new(&id);
            assert_eq!(kill(&id), Ok(()), "registered while guard alive");
        }
        assert_eq!(
            kill(&id),
            Err("session not found".to_string()),
            "guard drop unregisters"
        );
    }

    #[test]
    fn kill_unknown_id_is_session_not_found() {
        let reg = Registry::new();
        let err = reg.kill("test-reg-no-such-id").unwrap_err();
        assert_eq!(err, "session not found");
    }

    /// kill_all drains the registry (fake pids make killpg a harmless
    /// ESRCH no-op) and reports how many groups were targeted. The
    /// instance is private to this test.
    #[test]
    fn kill_all_drains_registry() {
        let reg = Registry::new();
        let id = "test-ka-1";
        reg.register(id, 1 << 20).unwrap();
        reg.register("test-ka-2", 1 << 21).unwrap();
        assert_eq!(reg.kill_all(), 2);
        assert_eq!(reg.kill(id), Err("session not found".to_string()));
        assert_eq!(reg.kill_all(), 0);
    }
}

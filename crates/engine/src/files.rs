//! File operations via SandboxAdapter.

#![allow(dead_code)]

use adapter_traits::{ExecCommand, SandboxHandle};
use futures::executor::block_on;

/// Read a file from inside the sandbox.
pub fn read_file(sb: &dyn SandboxHandle, path: &str) -> Result<String, String> {
    let result = block_on(sb.exec(&ExecCommand {
        args: vec!["cat".into(), path.into()],
        work_dir: None,
        env: None,
    }))
    .map_err(|e| format!("read_file: {}", e))?;

    if result.exit_code != 0 {
        return Err(format!(
            "read_file '{}' failed (exit {}): {}",
            path, result.exit_code, result.stderr
        ));
    }
    Ok(result.stdout)
}

/// Write content to a file inside the sandbox.
pub fn write_file(sb: &dyn SandboxHandle, path: &str, content: &str) -> Result<(), String> {
    let script = format!("cat > '{}' << 'TERRA_EOF'\n{}TERRA_EOF", path, content);
    let result = block_on(sb.exec(&ExecCommand {
        args: vec!["sh".into(), "-c".into(), script],
        work_dir: None,
        env: None,
    }))
    .map_err(|e| format!("write_file: {}", e))?;

    if result.exit_code != 0 {
        return Err(format!(
            "write_file '{}' failed (exit {}): {}",
            path, result.exit_code, result.stderr
        ));
    }
    Ok(())
}

/// List directory contents inside the sandbox.
pub fn list_dir(sb: &dyn SandboxHandle, path: &str) -> Result<Vec<String>, String> {
    let result = block_on(sb.exec(&ExecCommand {
        args: vec!["ls".into(), "-1".into(), path.into()],
        work_dir: None,
        env: None,
    }))
    .map_err(|e| format!("list_dir: {}", e))?;

    if result.exit_code != 0 {
        return Err(format!(
            "list_dir '{}' failed (exit {}): {}",
            path, result.exit_code, result.stderr
        ));
    }
    Ok(result.stdout.lines().map(|s| s.to_string()).collect())
}

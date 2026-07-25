//! Tool environment initialization.

#![allow(dead_code)]

use adapter_traits::{ExecCommand, SandboxHandle};
use futures::executor::block_on;

/// Synchronous wrapper: initialize Python venv.
pub fn init_python(sb: &dyn SandboxHandle) -> Result<(), String> {
    let result = block_on(sb.exec(&ExecCommand {
        args: vec![
            "python3".into(),
            "-m".into(),
            "venv".into(),
            "/home/agent/venv".into(),
        ],
        work_dir: None,
        env: None,
    }))
    .map_err(|e| format!("venv: {}", e))?;

    if result.exit_code != 0 {
        return Err(format!("venv failed: {}", result.stderr));
    }
    Ok(())
}

/// Synchronous wrapper: initialize Node.js npm prefix.
pub fn init_node(sb: &dyn SandboxHandle) -> Result<(), String> {
    let result = block_on(sb.exec(&ExecCommand {
        args: vec!["mkdir".into(), "-p".into(), "/home/agent/npm".into()],
        work_dir: None,
        env: None,
    }))
    .map_err(|e| format!("npm prefix: {}", e))?;

    if result.exit_code != 0 {
        return Err(format!("npm prefix failed: {}", result.stderr));
    }
    Ok(())
}

/// Initialize tools for a sandbox. Returns errors for any that failed.
pub fn init_tools(sb: &dyn SandboxHandle, tools: &[String]) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    for tool in tools {
        let result = match tool.as_str() {
            "python" | "python3" => init_python(sb),
            "node" | "nodejs" => init_node(sb),
            _ => continue,
        };
        if let Err(e) = result {
            errors.push(format!("{}: {}", tool, e));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

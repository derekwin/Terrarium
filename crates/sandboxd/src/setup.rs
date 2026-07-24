//! Sandbox environment setup: virtualenv + npm prefix initialization.

use std::path::Path;
use std::process::Command;

pub const AGENT_HOME: &str = "/home/agent";

pub struct SandboxEnv {
    pub activate_script: String,
}

/// Initialize sandbox tools. Idempotent — skips already-initialized tools.
pub fn setup_tools(python: bool, node: bool) -> Result<SandboxEnv, String> {
    std::fs::create_dir_all(AGENT_HOME).map_err(|e| format!("mkdir agent home: {}", e))?;

    let venv_path = format!("{}/venv", AGENT_HOME);
    let npm_prefix = format!("{}/npm", AGENT_HOME);

    if python && !Path::new(&venv_path).join("bin/python3").exists() {
        let output = Command::new("python3")
            .args(["-m", "venv", &venv_path])
            .output()
            .map_err(|e| format!("python3 -m venv: {}", e))?;
        if !output.status.success() {
            return Err(format!(
                "venv creation failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }

    if node {
        std::fs::create_dir_all(&npm_prefix).map_err(|e| format!("mkdir npm prefix: {}", e))?;
    }

    // Rewrite activation script with current tool state
    let mut activate = String::from("# Terrarium sandbox activation\n");
    if python {
        activate.push_str(&format!(
            "export VIRTUAL_ENV=\"{venv}\"\n",
            venv = venv_path
        ));
        activate.push_str(&format!(
            "export PATH=\"{venv}/bin:$PATH\"\n",
            venv = venv_path
        ));
    }
    if node {
        activate.push_str(&format!(
            "export NPM_CONFIG_PREFIX=\"{npm}\"\n",
            npm = npm_prefix
        ));
        activate.push_str(&format!(
            "export PATH=\"{npm}/bin:$PATH\"\n",
            npm = npm_prefix
        ));
    }
    activate.push_str(&format!(
        "export AGENT_HOME=\"{home}\"\n",
        home = AGENT_HOME
    ));

    let activate_path = format!("{}/activate", AGENT_HOME);
    std::fs::write(&activate_path, &activate).map_err(|e| format!("write activate: {}", e))?;

    Ok(SandboxEnv {
        activate_script: activate_path,
    })
}
